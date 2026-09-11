//! Frozen Zerodha NSE-options backtest costs in integer micro-rupees.
//!
//! This is a portable copy of the approved `zerodha_options_nse_backtest_v1`
//! kernel. The schedule is a research sensitivity, not a historical contract
//! note or a claim about the data owner's broker.

use backtest_contracts::{Money, Side};
use backtest_engine::{CostModel, EngineError, FeeContext, PricedLeg};

pub const BROKER_ID: &str = "zerodha";
pub const SCHEDULE_ID: &str = "zerodha_options_nse_backtest_v1";
pub const SCHEDULE_SHA256: &str =
    "80f00b252f4bf365779c3c93b1436e20af3a8fba938d51957d99ba837934b9d3";

const MICRO: i128 = 1_000_000;
const BROKERAGE: i128 = 20 * MICRO;

#[derive(Clone, Copy, Debug, Default)]
pub struct ZerodhaCosts;

const fn arithmetic() -> EngineError {
    EngineError::ArithmeticOverflow
}

fn round_even(numerator: i128, denominator: i128) -> Result<i128, EngineError> {
    if numerator < 0 || denominator <= 0 {
        return Err(arithmetic());
    }
    let quotient = numerator / denominator;
    let remainder = numerator % denominator;
    let doubled = remainder.checked_mul(2).ok_or_else(arithmetic)?;
    if doubled > denominator || (doubled == denominator && quotient % 2 != 0) {
        quotient.checked_add(1).ok_or_else(arithmetic)
    } else {
        Ok(quotient)
    }
}

fn fee_micro(leg: &PricedLeg) -> Result<i64, EngineError> {
    if leg.price.0 <= 0 || leg.quantity == 0 {
        return Err(EngineError::InvalidCost(Money(0)));
    }
    let notional = i128::from(leg.price.0)
        .checked_mul(i128::from(leg.quantity))
        .ok_or_else(arithmetic)?;
    let exchange = round_even(
        notional.checked_mul(3_553).ok_or_else(arithmetic)?,
        10_000_000,
    )?;
    let stt = if leg.side == Side::Sell {
        round_even(notional, 1_000)?
    } else {
        0
    };
    let sebi = round_even(notional, 1_000_000)?;
    let gst_numerator = BROKERAGE
        .checked_mul(10_000_000)
        .and_then(|value| {
            notional
                .checked_mul(3_563)
                .and_then(|charges| value.checked_add(charges))
        })
        .and_then(|value| value.checked_mul(18))
        .ok_or_else(arithmetic)?;
    let gst = round_even(gst_numerator, 1_000_000_000)?;
    let stamp = if leg.side == Side::Buy {
        round_even(notional.checked_mul(3).ok_or_else(arithmetic)?, 100_000)?
    } else {
        0
    };
    let total = [BROKERAGE, exchange, stt, sebi, gst, stamp]
        .into_iter()
        .try_fold(0_i128, |sum, value| {
            sum.checked_add(value).ok_or_else(arithmetic)
        })?;
    i64::try_from(total).map_err(|_| arithmetic())
}

impl CostModel for ZerodhaCosts {
    fn fee(&self, leg: &PricedLeg) -> Result<Money, EngineError> {
        Ok(Money(fee_micro(leg)?))
    }

    fn fee_with_context(
        &self,
        _context: &FeeContext<'_>,
        leg: &PricedLeg,
    ) -> Result<Money, EngineError> {
        self.fee(leg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leg(side: Side) -> PricedLeg {
        PricedLeg {
            instrument_id: "NIFTY|fixture".into(),
            side,
            quantity: 25,
            price: Money(100_000_000),
        }
    }

    #[test]
    fn frozen_vectors_match_approved_kernel() {
        assert_eq!(
            ZerodhaCosts.fee(&leg(Side::Sell)).unwrap(),
            Money(27_151_085)
        );
        assert_eq!(
            ZerodhaCosts.fee(&leg(Side::Buy)).unwrap(),
            Money(24_726_085)
        );
    }

    #[test]
    fn rejects_zero_quantity_and_price() {
        let mut value = leg(Side::Buy);
        value.quantity = 0;
        assert!(ZerodhaCosts.fee(&value).is_err());
        value.quantity = 1;
        value.price = Money(0);
        assert!(ZerodhaCosts.fee(&value).is_err());
    }
}
