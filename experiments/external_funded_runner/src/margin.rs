//! Explicit external margin lanes.

use backtest_contracts::{AccountState, IntentAction, Money, SealedEvent, Side, TradeIntent};
use backtest_engine::{MarginProvider, PortfolioMargin};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MarginMode {
    PremiumOnly,
    Authoritative,
    UnfundedZeroSensitivity,
}

#[derive(Clone, Copy, Debug)]
pub struct EventMargin {
    mode: MarginMode,
}

impl EventMargin {
    pub const fn new(mode: MarginMode) -> Self {
        Self { mode }
    }
}

impl MarginProvider for EventMargin {
    fn required_margin(
        &self,
        event: &SealedEvent,
        intent: &TradeIntent,
        _account: &AccountState,
    ) -> Result<Money, String> {
        if !matches!(intent.action, IntentAction::Open)
            || intent.legs.iter().all(|leg| leg.side == Side::Buy)
            || self.mode == MarginMode::UnfundedZeroSensitivity
        {
            return Ok(Money::ZERO);
        }
        if self.mode == MarginMode::PremiumOnly {
            return Err("opening sell leg is outside premium-only margin authority".into());
        }
        event
            .margin_facts
            .get(&intent.basket_key)
            .map(|fact| fact.required)
            .ok_or_else(|| {
                format!(
                    "missing authoritative opening margin for basket {}",
                    intent.basket_key
                )
            })
    }

    fn refresh_portfolio_margin(
        &self,
        event: &SealedEvent,
        _account: &AccountState,
    ) -> Result<PortfolioMargin, String> {
        if self.mode != MarginMode::Authoritative {
            return Ok(PortfolioMargin::Unsupported);
        }
        Ok(event
            .margin_facts
            .get("__portfolio__")
            .map_or(PortfolioMargin::Missing, |fact| {
                PortfolioMargin::Available(fact.required)
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use backtest_contracts::{CONTRACT_VERSION, IntentLeg, MarginFact};
    use std::collections::BTreeMap;

    fn event() -> SealedEvent {
        SealedEvent {
            schema_version: CONTRACT_VERSION.into(),
            event_id: "e".into(),
            sequence: 0,
            decision_at_ns: 1,
            available_at_ns: 1,
            sealed_at_ns: 1,
            quotes: BTreeMap::new(),
            margin_facts: BTreeMap::from([(
                "b".into(),
                MarginFact {
                    fact_id: "m".into(),
                    basket_key: "b".into(),
                    required: Money(123),
                    observed_at_ns: 1,
                    available_at_ns: 1,
                    source_id: "owner".into(),
                },
            )]),
            research_payload: serde_json::json!({}),
        }
    }

    fn intent(side: Side) -> TradeIntent {
        TradeIntent {
            schema_version: CONTRACT_VERSION.into(),
            intent_id: "i".into(),
            decision_id: "d".into(),
            strategy_position_id: "p".into(),
            basket_key: "b".into(),
            action: IntentAction::Open,
            atomic: true,
            legs: vec![IntentLeg {
                instrument_id: "x".into(),
                side,
                quantity: 1,
                limit_price: None,
            }],
            lineage: BTreeMap::new(),
        }
    }

    #[test]
    fn premium_only_admits_pure_long_and_rejects_sell_leg() {
        let provider = EventMargin::new(MarginMode::PremiumOnly);
        assert_eq!(
            provider
                .required_margin(&event(), &intent(Side::Buy), &account())
                .unwrap(),
            Money::ZERO
        );
        assert!(
            provider
                .required_margin(&event(), &intent(Side::Sell), &account())
                .is_err()
        );
    }

    #[test]
    fn authoritative_short_uses_exact_basket_fact() {
        let provider = EventMargin::new(MarginMode::Authoritative);
        assert_eq!(
            provider
                .required_margin(&event(), &intent(Side::Sell), &account())
                .unwrap(),
            Money(123)
        );
        let empty = SealedEvent {
            margin_facts: BTreeMap::new(),
            ..event()
        };
        assert!(
            provider
                .required_margin(&empty, &intent(Side::Sell), &account())
                .is_err()
        );
    }

    #[test]
    fn zero_margin_lane_is_explicitly_unfunded() {
        assert_eq!(
            EventMargin::new(MarginMode::UnfundedZeroSensitivity)
                .required_margin(&event(), &intent(Side::Sell), &account())
                .unwrap(),
            Money::ZERO
        );
    }

    fn account() -> AccountState {
        AccountState {
            cash: Money(1_000),
            reserved_margin: Money::ZERO,
            realized_pnl: Money::ZERO,
            unrealized_pnl: Money::ZERO,
            fees_paid: Money::ZERO,
            equity: Money(1_000),
            positions: Vec::new(),
        }
    }
}
