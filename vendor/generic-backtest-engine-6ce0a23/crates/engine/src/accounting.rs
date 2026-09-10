use std::collections::BTreeMap;

use backtest_contracts::{AccountState, Fill, MarketQuote, Money, PositionView, QuoteUse, Side};
use serde::{Deserialize, Serialize};

use crate::EngineError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Position {
    quantity: i64,
    cost_basis: Money,
    mark_price: Money,
    realized_pnl: Money,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountLedger {
    initial_cash: Money,
    cash: Money,
    realized_pnl: Money,
    fees_paid: Money,
    positions: BTreeMap<String, BTreeMap<String, Position>>,
    reservations: BTreeMap<String, Money>,
    reserved_margin: Money,
    /// When present, `reserved_margin` is an authoritative provider total.
    /// Position reservations are retained only for the legacy incremental
    /// margin mode and are not added on top of this value.
    #[serde(default)]
    authoritative_margin: Option<Money>,
}

impl AccountLedger {
    #[must_use]
    pub const fn new(initial_cash: Money) -> Self {
        Self {
            initial_cash,
            cash: initial_cash,
            realized_pnl: Money::ZERO,
            fees_paid: Money::ZERO,
            positions: BTreeMap::new(),
            reservations: BTreeMap::new(),
            reserved_margin: Money::ZERO,
            authoritative_margin: None,
        }
    }

    #[must_use]
    pub const fn initial_cash(&self) -> Money {
        self.initial_cash
    }

    #[must_use]
    pub const fn cash(&self) -> Money {
        self.cash
    }

    #[must_use]
    pub const fn reserved_margin(&self) -> Money {
        self.reserved_margin
    }

    #[must_use]
    pub const fn has_authoritative_margin(&self) -> bool {
        self.authoritative_margin.is_some()
    }

    /// Adds collateral reserved for a strategy position.
    ///
    /// # Errors
    ///
    /// Returns an error for negative margin or integer overflow.
    pub fn reserve(&mut self, position_id: &str, amount: Money) -> Result<(), EngineError> {
        if amount.0 < 0 {
            return Err(EngineError::InvalidMargin(amount));
        }
        if self.authoritative_margin.is_some() {
            // A provider total already includes every held position. Until a
            // post-fill refresh arrives, this is only a conservative
            // provisional increment for admission; it must not be added to a
            // second provider total.
            self.reserved_margin = self
                .reserved_margin
                .checked_add(amount)
                .ok_or(EngineError::ArithmeticOverflow)?;
            return Ok(());
        }
        let current = self
            .reservations
            .get(position_id)
            .copied()
            .unwrap_or(Money::ZERO);
        let updated = current
            .checked_add(amount)
            .ok_or(EngineError::ArithmeticOverflow)?;
        self.reserved_margin = self
            .reserved_margin
            .checked_add(amount)
            .ok_or(EngineError::ArithmeticOverflow)?;
        self.reservations.insert(position_id.to_owned(), updated);
        Ok(())
    }

    /// Replaces collateral reserved for a strategy position.
    ///
    /// # Errors
    ///
    /// Returns an error for negative margin.
    pub fn set_reservation(&mut self, position_id: &str, amount: Money) -> Result<(), EngineError> {
        if amount.0 < 0 {
            return Err(EngineError::InvalidMargin(amount));
        }
        if self.authoritative_margin.is_some() {
            // The total provider quote has no safe per-position decomposition.
            // Keep lifecycle release conservative and let the next refresh
            // determine the exact portfolio total.
            if amount == Money::ZERO {
                self.reservations.remove(position_id);
            }
            return Ok(());
        }
        let current = self.reservation(position_id);
        self.reserved_margin = self
            .reserved_margin
            .checked_sub(current)
            .and_then(|total| total.checked_add(amount))
            .ok_or(EngineError::ArithmeticOverflow)?;
        if amount == Money::ZERO {
            self.reservations.remove(position_id);
        } else {
            self.reservations.insert(position_id.to_owned(), amount);
        }
        Ok(())
    }

    #[must_use]
    pub fn reservation(&self, position_id: &str) -> Money {
        self.reservations
            .get(position_id)
            .copied()
            .unwrap_or(Money::ZERO)
    }

    /// Replaces all legacy reservations with one authoritative provider total.
    ///
    /// # Errors
    ///
    /// Returns an error for a negative margin amount.
    pub fn set_authoritative_margin(&mut self, amount: Money) -> Result<(), EngineError> {
        if amount.0 < 0 {
            return Err(EngineError::InvalidMargin(amount));
        }
        self.reserved_margin = amount;
        self.authoritative_margin = Some(amount);
        self.reservations.clear();
        Ok(())
    }

    /// Conservatively releases an authoritative total once no positions are
    /// held. A partial close retains the prior liability until the provider
    /// supplies a fresh total.
    pub fn release_authoritative_if_flat(&mut self) {
        if self.authoritative_margin.is_some() && self.positions.is_empty() {
            self.reserved_margin = Money::ZERO;
            self.authoritative_margin = Some(Money::ZERO);
            self.reservations.clear();
        }
    }

    #[must_use]
    pub fn gross_quantity(&self, position_id: &str) -> u128 {
        self.positions
            .get(position_id)
            .into_iter()
            .flat_map(BTreeMap::values)
            .map(|position| u128::from(position.quantity.unsigned_abs()))
            .sum()
    }

    #[must_use]
    pub fn quantity(&self, position_id: &str, instrument_id: &str) -> i64 {
        self.positions
            .get(position_id)
            .and_then(|positions| positions.get(instrument_id))
            .map_or(0, |position| position.quantity)
    }

    #[must_use]
    pub fn position_quantities(&self, position_id: &str) -> BTreeMap<String, i64> {
        self.positions
            .get(position_id)
            .map_or_else(BTreeMap::new, |positions| {
                positions
                    .iter()
                    .map(|(instrument, position)| (instrument.clone(), position.quantity))
                    .collect()
            })
    }

    /// Applies one fee-bearing fill to cash and position state.
    ///
    /// # Errors
    ///
    /// Returns an error if monetary or quantity arithmetic overflows.
    pub fn apply_fill(&mut self, position_id: &str, fill: &Fill) -> Result<(), EngineError> {
        let fill_quantity =
            i64::try_from(fill.quantity).map_err(|_| EngineError::ArithmeticOverflow)?;
        let signed_fill = match fill.side {
            Side::Buy => fill_quantity,
            Side::Sell => -fill_quantity,
        };
        let notional = checked_product(fill.price, signed_fill)?;
        self.cash = self
            .cash
            .checked_sub(notional)
            .and_then(|cash| cash.checked_sub(fill.fee))
            .ok_or(EngineError::ArithmeticOverflow)?;
        self.fees_paid = self
            .fees_paid
            .checked_add(fill.fee)
            .ok_or(EngineError::ArithmeticOverflow)?;

        let should_remove = {
            let position = self
                .positions
                .entry(position_id.to_owned())
                .or_default()
                .entry(fill.instrument_id.clone())
                .or_insert(Position {
                    quantity: 0,
                    cost_basis: Money::ZERO,
                    mark_price: fill.price,
                    realized_pnl: Money::ZERO,
                });
            let old_quantity = position.quantity;
            let new_quantity = old_quantity
                .checked_add(signed_fill)
                .ok_or(EngineError::ArithmeticOverflow)?;

            if old_quantity == 0 || old_quantity.signum() == signed_fill.signum() {
                let added_basis = checked_product(fill.price, fill_quantity)?;
                position.cost_basis = position
                    .cost_basis
                    .checked_add(added_basis)
                    .ok_or(EngineError::ArithmeticOverflow)?;
            } else {
                if fill.quantity > old_quantity.unsigned_abs() {
                    return Err(EngineError::InvalidIntent(
                        "fill would reverse a position inside accounting".to_owned(),
                    ));
                }
                let closed_quantity = old_quantity.unsigned_abs().min(fill.quantity);
                let allocated_basis = proportional_basis(
                    position.cost_basis,
                    closed_quantity,
                    old_quantity.unsigned_abs(),
                )?;
                let exit_notional = checked_product(
                    fill.price,
                    i64::try_from(closed_quantity).map_err(|_| EngineError::ArithmeticOverflow)?,
                )?;
                let realized = if old_quantity > 0 {
                    exit_notional.checked_sub(allocated_basis)
                } else {
                    allocated_basis.checked_sub(exit_notional)
                }
                .ok_or(EngineError::ArithmeticOverflow)?;
                position.realized_pnl = position
                    .realized_pnl
                    .checked_add(realized)
                    .ok_or(EngineError::ArithmeticOverflow)?;
                self.realized_pnl = self
                    .realized_pnl
                    .checked_add(realized)
                    .ok_or(EngineError::ArithmeticOverflow)?;
                position.cost_basis = position
                    .cost_basis
                    .checked_sub(allocated_basis)
                    .ok_or(EngineError::ArithmeticOverflow)?;
            }
            position.quantity = new_quantity;
            position.mark_price = fill.price;
            new_quantity == 0
        };
        if should_remove && let Some(positions) = self.positions.get_mut(position_id) {
            positions.remove(&fill.instrument_id);
            if positions.is_empty() {
                self.positions.remove(position_id);
            }
        }
        Ok(())
    }

    pub fn apply_marks(
        &mut self,
        quotes: &BTreeMap<String, MarketQuote>,
        event_available_at_ns: i64,
    ) {
        for positions in self.positions.values_mut() {
            for (instrument_id, position) in positions {
                let Some(quote) = quotes.get(instrument_id) else {
                    continue;
                };
                if quote.available_at_ns <= event_available_at_ns
                    && quote.allowed_uses.contains(&QuoteUse::Accounting)
                    && let Some(mark) = quote.mark
                    && mark.0 > 0
                {
                    position.mark_price = mark;
                }
            }
        }
    }

    #[must_use]
    pub fn accounting_mark_blockers(
        &self,
        quotes: &BTreeMap<String, MarketQuote>,
        event_available_at_ns: i64,
    ) -> Vec<String> {
        self.accounting_mark_blockers_with_max_age(quotes, event_available_at_ns, None)
    }

    /// Returns held positions whose accounting mark is unavailable or older
    /// than the caller's explicit admission bound.
    ///
    /// `None` preserves the legacy availability-only check.  A bound is
    /// deliberately opt-in: the engine must not silently impose a freshness
    /// policy on existing callers.
    #[must_use]
    pub fn accounting_mark_blockers_with_max_age(
        &self,
        quotes: &BTreeMap<String, MarketQuote>,
        event_available_at_ns: i64,
        max_age_ns: Option<u64>,
    ) -> Vec<String> {
        let mut blockers = Vec::new();
        for instruments in self.positions.values() {
            for instrument_id in instruments.keys() {
                let Some(quote) = quotes.get(instrument_id) else {
                    blockers.push(format!(
                        "missing current accounting mark for {instrument_id}"
                    ));
                    continue;
                };
                let available = quote.available_at_ns <= event_available_at_ns;
                let authorized = available
                    && quote.allowed_uses.contains(&QuoteUse::Accounting)
                    && quote.mark.is_some_and(|mark| mark.0 > 0);
                if !authorized {
                    blockers.push(format!(
                        "missing current accounting mark for {instrument_id}"
                    ));
                    continue;
                }
                if let Some(max_age_ns) = max_age_ns {
                    let age_ns = event_available_at_ns
                        .checked_sub(quote.observed_at_ns)
                        .and_then(|age| u64::try_from(age).ok());
                    if age_ns.is_none_or(|age| age > max_age_ns) {
                        let age =
                            age_ns.map_or_else(|| "unknown".to_owned(), |value| value.to_string());
                        blockers.push(format!(
                            "stale current accounting mark for {instrument_id} (age_ns={age})"
                        ));
                    }
                }
            }
        }
        blockers
    }

    /// Projects the deterministic public account state.
    ///
    /// # Errors
    ///
    /// Returns an error if market-value or P&L arithmetic overflows.
    pub fn state(&self) -> Result<AccountState, EngineError> {
        let reserved_margin = self.reserved_margin();
        let mut unrealized = Money::ZERO;
        let mut market_value = Money::ZERO;
        let position_count = self.positions.values().map(BTreeMap::len).sum();
        let mut positions = Vec::with_capacity(position_count);
        for (position_id, instruments) in &self.positions {
            for (instrument_id, position) in instruments {
                let absolute_quantity = position.quantity.unsigned_abs();
                let marked_notional = checked_product(
                    position.mark_price,
                    i64::try_from(absolute_quantity)
                        .map_err(|_| EngineError::ArithmeticOverflow)?,
                )?;
                let position_unrealized = if position.quantity > 0 {
                    marked_notional.checked_sub(position.cost_basis)
                } else {
                    position.cost_basis.checked_sub(marked_notional)
                }
                .ok_or(EngineError::ArithmeticOverflow)?;
                unrealized = unrealized
                    .checked_add(position_unrealized)
                    .ok_or(EngineError::ArithmeticOverflow)?;
                market_value = market_value
                    .checked_add(checked_product(position.mark_price, position.quantity)?)
                    .ok_or(EngineError::ArithmeticOverflow)?;
                positions.push(PositionView {
                    strategy_position_id: position_id.clone(),
                    instrument_id: instrument_id.clone(),
                    quantity: position.quantity,
                    average_price: Money(
                        position.cost_basis.0
                            / i64::try_from(absolute_quantity)
                                .map_err(|_| EngineError::ArithmeticOverflow)?,
                    ),
                    mark_price: position.mark_price,
                    realized_pnl: position.realized_pnl,
                    unrealized_pnl: position_unrealized,
                });
            }
        }
        let equity = self
            .cash
            .checked_add(market_value)
            .ok_or(EngineError::ArithmeticOverflow)?;
        Ok(AccountState {
            cash: self.cash,
            reserved_margin,
            realized_pnl: self.realized_pnl,
            unrealized_pnl: unrealized,
            fees_paid: self.fees_paid,
            equity,
            positions,
        })
    }
}

pub fn checked_product(money: Money, multiplier: i64) -> Result<Money, EngineError> {
    let product = i128::from(money.0) * i128::from(multiplier);
    Ok(Money(
        i64::try_from(product).map_err(|_| EngineError::ArithmeticOverflow)?,
    ))
}

fn proportional_basis(
    basis: Money,
    numerator: u64,
    denominator: u64,
) -> Result<Money, EngineError> {
    if denominator == 0 {
        return Err(EngineError::ArithmeticOverflow);
    }
    let allocated = i128::from(basis.0)
        .checked_mul(i128::from(numerator))
        .ok_or(EngineError::ArithmeticOverflow)?
        / i128::from(denominator);
    Ok(Money(
        i64::try_from(allocated).map_err(|_| EngineError::ArithmeticOverflow)?,
    ))
}
