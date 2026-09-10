use std::collections::BTreeMap;

use backtest_contracts::{
    AccountState, Fill, IntentLeg, MarketQuote, Money, QuoteUse, SealedEvent, Side, TradeIntent,
};
use serde::{Deserialize, Serialize};

use crate::EngineError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PricedLeg {
    pub instrument_id: String,
    pub side: Side,
    pub quantity: u64,
    pub price: Money,
}

pub trait PricingPolicy {
    /// Resolves one executable leg price from an authority-labelled event.
    ///
    /// # Errors
    ///
    /// Returns a reason when a quote is absent, future, unauthorized, or outside the limit.
    fn price(&self, event: &SealedEvent, leg: &IntentLeg) -> Result<PricedLeg, String>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CrossSpreadPricing;

impl PricingPolicy for CrossSpreadPricing {
    fn price(&self, event: &SealedEvent, leg: &IntentLeg) -> Result<PricedLeg, String> {
        let quote = event
            .quotes
            .get(&leg.instrument_id)
            .ok_or_else(|| format!("missing quote for {}", leg.instrument_id))?;
        validate_execution_quote(event, quote)?;
        let price = match leg.side {
            Side::Buy => quote.ask,
            Side::Sell => quote.bid,
        }
        .ok_or_else(|| format!("missing executable side for {}", leg.instrument_id))?;
        if price.0 <= 0 {
            return Err(format!(
                "non-positive executable price for {}",
                leg.instrument_id
            ));
        }
        if let Some(limit) = leg.limit_price {
            let outside_limit = match leg.side {
                Side::Buy => price.0 > limit.0,
                Side::Sell => price.0 < limit.0,
            };
            if outside_limit {
                return Err(format!(
                    "executable price outside limit for {}",
                    leg.instrument_id
                ));
            }
        }
        Ok(PricedLeg {
            instrument_id: leg.instrument_id.clone(),
            side: leg.side,
            quantity: leg.quantity,
            price,
        })
    }
}

fn validate_execution_quote(event: &SealedEvent, quote: &MarketQuote) -> Result<(), String> {
    if quote.available_at_ns > event.available_at_ns {
        return Err(format!("future quote for {}", quote.instrument_id));
    }
    if !quote.allowed_uses.contains(&QuoteUse::Execution) {
        return Err(format!(
            "quote lacks execution authority for {}",
            quote.instrument_id
        ));
    }
    Ok(())
}

pub trait CostModel {
    /// Computes the fee for one priced fill.
    ///
    /// # Errors
    ///
    /// Returns an error when cost configuration or arithmetic is invalid.
    fn fee(&self, leg: &PricedLeg) -> Result<Money, EngineError>;

    /// Computes the conservative cash reserve used only for pre-trade
    /// admission. By default this is the executable fee estimate, preserving
    /// legacy behavior. Brokers may override it when their basket-margin API
    /// reserves more cash than the eventual statutory charges.
    ///
    /// # Errors
    ///
    /// Returns an error when reserve configuration or arithmetic is invalid.
    fn admission_reserve(&self, leg: &PricedLeg) -> Result<Money, EngineError> {
        self.fee(leg)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinearCostModel {
    pub basis_points: u32,
    pub fixed_per_fill: Money,
}

impl CostModel for LinearCostModel {
    fn fee(&self, leg: &PricedLeg) -> Result<Money, EngineError> {
        if self.fixed_per_fill.0 < 0 {
            return Err(EngineError::InvalidCost(self.fixed_per_fill));
        }
        let notional = i128::from(leg.price.0)
            .checked_mul(i128::from(leg.quantity))
            .ok_or(EngineError::ArithmeticOverflow)?;
        let variable = notional
            .checked_mul(i128::from(self.basis_points))
            .ok_or(EngineError::ArithmeticOverflow)?
            / 10_000;
        let total = variable
            .checked_add(i128::from(self.fixed_per_fill.0))
            .ok_or(EngineError::ArithmeticOverflow)?;
        Ok(Money(
            i64::try_from(total).map_err(|_| EngineError::ArithmeticOverflow)?,
        ))
    }
}

pub trait MarginProvider {
    /// Returns the incremental post-fill collateral reservation for this
    /// opening intent. The engine projects fill cash flows and fees separately
    /// when deciding whether the account can fund the trade.
    ///
    /// # Errors
    ///
    /// The engine calls this again with the actually filled subset after a
    /// partial execution. Returns a reason when an authoritative quote is
    /// unavailable.
    fn required_margin(
        &self,
        event: &SealedEvent,
        intent: &TradeIntent,
        account: &AccountState,
    ) -> Result<Money, String>;

    /// Returns replacement collateral allocations for currently held strategy
    /// positions at the current causal event.
    ///
    /// The map must contain exactly one entry for every currently held
    /// `strategy_position_id`; values are required collateral in integer money
    /// units. `None` means that held-position refresh is unavailable. The
    /// default keeps existing providers backward compatible.
    ///
    /// # Errors
    ///
    /// Returns a reason when current held-position collateral is unavailable.
    fn held_margin(
        &self,
        _event: &SealedEvent,
        _account: &AccountState,
    ) -> Result<Option<BTreeMap<String, Money>>, String> {
        Ok(None)
    }

    /// Returns one account-level collateral requirement for the complete held
    /// portfolio at the current causal event. Providers with nonlinear
    /// portfolio offsets should use this instead of assigning aggregate margin
    /// to an arbitrary strategy position. `None` falls back to `held_margin`.
    ///
    /// # Errors
    ///
    /// Returns a reason when current portfolio collateral is unavailable.
    fn held_portfolio_margin(
        &self,
        _event: &SealedEvent,
        _account: &AccountState,
    ) -> Result<Option<Money>, String> {
        Ok(None)
    }
}

pub trait ExecutionModel {
    fn execute(
        &mut self,
        event: &SealedEvent,
        intent: &TradeIntent,
        legs: &[PricedLeg],
    ) -> RawExecution;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawFill {
    pub instrument_id: String,
    pub side: Side,
    pub quantity: u64,
    pub price: Money,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawExecution {
    pub fills: Vec<RawFill>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ImmediateExecution;

impl ExecutionModel for ImmediateExecution {
    fn execute(
        &mut self,
        _event: &SealedEvent,
        _intent: &TradeIntent,
        legs: &[PricedLeg],
    ) -> RawExecution {
        RawExecution {
            fills: legs
                .iter()
                .map(|leg| RawFill {
                    instrument_id: leg.instrument_id.clone(),
                    side: leg.side,
                    quantity: leg.quantity,
                    price: leg.price,
                })
                .collect(),
            reason: None,
        }
    }
}

pub fn apply_costs(
    raw_fills: Vec<RawFill>,
    costs: &impl CostModel,
) -> Result<Vec<Fill>, EngineError> {
    raw_fills
        .into_iter()
        .map(|fill| {
            let priced = PricedLeg {
                instrument_id: fill.instrument_id.clone(),
                side: fill.side,
                quantity: fill.quantity,
                price: fill.price,
            };
            Ok(Fill {
                instrument_id: fill.instrument_id,
                side: fill.side,
                quantity: fill.quantity,
                price: fill.price,
                fee: costs.fee(&priced)?,
            })
        })
        .collect()
}
