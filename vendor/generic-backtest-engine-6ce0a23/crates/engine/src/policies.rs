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

/// A named hypothetical pricing policy using the event's close/mark price.
///
/// This policy uses a quote's mark only when that mark is explicitly
/// executable, available by the event, positive, and within the intent limit.
/// It is applicable to any intent action because the event close is the
/// hypothetical execution price for both entries and exits. Accounting-only
/// marks cannot silently become synthetic fills.
#[derive(Debug, Clone, Copy, Default)]
pub struct CloseMarkPricing;

impl PricingPolicy for CloseMarkPricing {
    fn price(&self, event: &SealedEvent, leg: &IntentLeg) -> Result<PricedLeg, String> {
        let quote = event
            .quotes
            .get(&leg.instrument_id)
            .ok_or_else(|| format!("missing quote for {}", leg.instrument_id))?;
        validate_execution_quote(event, quote)?;
        let price = quote
            .mark
            .ok_or_else(|| format!("missing executable mark for {}", leg.instrument_id))?;
        if price.0 <= 0 {
            return Err(format!(
                "non-positive executable mark for {}",
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
                    "executable mark outside limit for {}",
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

/// Identifies whether a fee is being used to admit an order or settle a fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeeStage {
    Estimate,
    Actual,
}

/// Context supplied to a fee model for one order leg.
///
/// `order_id` is the canonical intent identity. `order_fills` contains every
/// raw fill returned for that intent, allowing a contextual model to apply
/// order-level fees without changing the simple per-leg [`CostModel::fee`]
/// contract. During estimates there are no actual fills and `fill` is `None`.
#[derive(Debug, Clone, Copy)]
pub struct FeeContext<'a> {
    pub event: &'a SealedEvent,
    pub intent: &'a TradeIntent,
    pub order_id: &'a str,
    pub stage: FeeStage,
    pub fill: Option<&'a RawFill>,
    pub fill_index: Option<usize>,
    pub order_fills: &'a [RawFill],
}

impl<'a> FeeContext<'a> {
    #[must_use]
    pub const fn estimate(event: &'a SealedEvent, intent: &'a TradeIntent) -> Self {
        Self {
            event,
            intent,
            order_id: intent.intent_id.as_str(),
            stage: FeeStage::Estimate,
            fill: None,
            fill_index: None,
            order_fills: &[],
        }
    }

    #[must_use]
    pub const fn actual(
        event: &'a SealedEvent,
        intent: &'a TradeIntent,
        fill: &'a RawFill,
        fill_index: usize,
        order_fills: &'a [RawFill],
    ) -> Self {
        Self {
            event,
            intent,
            order_id: intent.intent_id.as_str(),
            stage: FeeStage::Actual,
            fill: Some(fill),
            fill_index: Some(fill_index),
            order_fills,
        }
    }
}

pub trait CostModel {
    /// Computes the fee for one priced fill.
    ///
    /// # Errors
    ///
    /// Returns an error when cost configuration or arithmetic is invalid.
    fn fee(&self, leg: &PricedLeg) -> Result<Money, EngineError>;

    /// Computes a fee with event, intent, order, and estimate/fill context.
    ///
    /// The default delegates to the original simple fee API, so existing
    /// implementations remain source-compatible.
    ///
    /// # Errors
    ///
    /// Returns the same validation or arithmetic error as [`Self::fee`].
    fn fee_with_context(
        &self,
        _context: &FeeContext<'_>,
        leg: &PricedLeg,
    ) -> Result<Money, EngineError> {
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
    /// Returns incremental collateral required for this opening intent.
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

    /// Refreshes the authoritative total margin for the current portfolio.
    ///
    /// The legacy default is [`PortfolioMargin::Unsupported`]. An opted-in
    /// engine treats `Missing` or `Unsupported` as unavailable current margin
    /// for new-risk admission, while preserving existing liability and still
    /// allowing risk-reducing intents.
    ///
    /// # Errors
    ///
    /// Returns a provider diagnostic when the refresh cannot be evaluated.
    fn refresh_portfolio_margin(
        &self,
        _event: &SealedEvent,
        _account: &AccountState,
    ) -> Result<PortfolioMargin, String> {
        Ok(PortfolioMargin::Unsupported)
    }
}

/// Result of an optional authoritative portfolio-margin refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortfolioMargin {
    /// The provider does not implement authoritative total margin.
    Unsupported,
    /// The provider supports the operation but has no current usable fact.
    Missing,
    /// The provider supplied the complete current portfolio requirement.
    Available(Money),
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

/// Applies simple per-fill costs to actual fills.
///
/// # Errors
///
/// Returns a cost or arithmetic error from the configured model.
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
            let fee = costs.fee(&priced)?;
            if fee.0 < 0 {
                return Err(EngineError::InvalidCost(fee));
            }
            Ok(Fill {
                instrument_id: fill.instrument_id,
                side: fill.side,
                quantity: fill.quantity,
                price: fill.price,
                fee,
            })
        })
        .collect()
}

/// Applies contextual fees to a complete order's actual fills.
///
/// The raw fill slice remains available to every fee call, so order-level fee
/// schedules can charge once per intent while simple models continue to charge
/// each fill independently.
///
/// # Errors
///
/// Returns a cost or arithmetic error from the configured model.
pub fn apply_costs_with_context(
    event: &SealedEvent,
    intent: &TradeIntent,
    raw_fills: &[RawFill],
    costs: &impl CostModel,
) -> Result<Vec<Fill>, EngineError> {
    raw_fills
        .iter()
        .enumerate()
        .map(|(fill_index, fill)| {
            let priced = PricedLeg {
                instrument_id: fill.instrument_id.clone(),
                side: fill.side,
                quantity: fill.quantity,
                price: fill.price,
            };
            let context = FeeContext::actual(event, intent, fill, fill_index, raw_fills);
            let fee = costs.fee_with_context(&context, &priced)?;
            if fee.0 < 0 {
                return Err(EngineError::InvalidCost(fee));
            }
            Ok(Fill {
                instrument_id: fill.instrument_id.clone(),
                side: fill.side,
                quantity: fill.quantity,
                price: fill.price,
                fee,
            })
        })
        .collect()
}
