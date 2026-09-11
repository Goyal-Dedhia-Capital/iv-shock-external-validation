//! Explicit liquidity and slippage execution sensitivities.

use std::collections::BTreeMap;

use backtest_contracts::{Money, SealedEvent, Side, TradeIntent};
use backtest_engine::{ExecutionModel, PricedLeg, RawExecution, RawFill};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CapacityMode {
    /// Quote presence only; no depth claim. This is an executable-spread upper bound.
    UnlimitedQuote,
    /// Require owner-attested top-of-book size for the aggressive side.
    TopOfBook,
    /// Activity-only stress test. Traded volume is not represented as observed depth.
    BarVolumeSensitivity,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionConfig {
    pub capacity_mode: CapacityMode,
    pub slippage_bps: u32,
    pub slippage_ticks: u32,
    pub volume_participation_bps: u32,
}

impl ExecutionConfig {
    pub fn validate(self) -> Result<Self, String> {
        if self.slippage_bps > 10_000 || self.volume_participation_bps > 10_000 {
            return Err("slippage and volume participation must be <= 10000 bps".into());
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LiquidityFact {
    source_kind: SourceKind,
    source_id: String,
    bid_size: Option<u64>,
    ask_size: Option<u64>,
    bar_volume: Option<u64>,
    tick_size_micro: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SourceKind {
    Observed,
}

#[derive(Debug)]
pub struct LiquidityExecution {
    config: ExecutionConfig,
}

impl LiquidityExecution {
    pub fn new(config: ExecutionConfig) -> Result<Self, String> {
        Ok(Self {
            config: config.validate()?,
        })
    }

    fn facts(event: &SealedEvent) -> Result<BTreeMap<String, LiquidityFact>, String> {
        serde_json::from_value(
            event
                .research_payload
                .get("execution_liquidity")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({})),
        )
        .map_err(|error| format!("invalid execution_liquidity payload: {error}"))
    }

    fn capacity(&self, side: Side, fact: &LiquidityFact) -> Option<u64> {
        match self.config.capacity_mode {
            CapacityMode::UnlimitedQuote => Some(u64::MAX),
            CapacityMode::TopOfBook => match side {
                Side::Buy => fact.ask_size,
                Side::Sell => fact.bid_size,
            },
            CapacityMode::BarVolumeSensitivity => fact.bar_volume.map(|volume| {
                u64::try_from(
                    u128::from(volume) * u128::from(self.config.volume_participation_bps) / 10_000,
                )
                .unwrap_or(u64::MAX)
            }),
        }
    }

    fn slipped_price(&self, leg: &PricedLeg, tick_size: i64) -> Result<Money, String> {
        if tick_size <= 0 {
            return Err("positive tick_size_micro is required".into());
        }
        let by_ticks = i128::from(tick_size)
            .checked_mul(i128::from(self.config.slippage_ticks))
            .ok_or("slippage arithmetic overflow")?;
        let by_bps_numerator = i128::from(leg.price.0)
            .checked_mul(i128::from(self.config.slippage_bps))
            .ok_or("slippage arithmetic overflow")?;
        let by_bps = by_bps_numerator
            .checked_add(9_999)
            .ok_or("slippage arithmetic overflow")?
            / 10_000;
        let impact = by_ticks.max(by_bps);
        let value = match leg.side {
            Side::Buy => i128::from(leg.price.0).checked_add(impact),
            Side::Sell => i128::from(leg.price.0).checked_sub(impact),
        }
        .ok_or("slippage arithmetic overflow")?;
        if value <= 0 {
            return Err("slippage produces a non-positive fill price".into());
        }
        Ok(Money(
            i64::try_from(value).map_err(|_| "slippage arithmetic overflow")?,
        ))
    }
}

impl ExecutionModel for LiquidityExecution {
    fn execute(
        &mut self,
        event: &SealedEvent,
        intent: &TradeIntent,
        legs: &[PricedLeg],
    ) -> RawExecution {
        let facts = match Self::facts(event) {
            Ok(value) => value,
            Err(reason) => {
                return RawExecution {
                    fills: vec![],
                    reason: Some(reason),
                };
            }
        };
        let mut fills = Vec::with_capacity(legs.len());
        for leg in legs {
            let Some(fact) = facts.get(&leg.instrument_id) else {
                return RawExecution {
                    fills: vec![],
                    reason: Some(format!("missing liquidity fact for {}", leg.instrument_id)),
                };
            };
            if fact.source_kind != SourceKind::Observed {
                return RawExecution {
                    fills: vec![],
                    reason: Some(format!(
                        "non-observed execution source for {}",
                        leg.instrument_id
                    )),
                };
            }
            let Some(quote) = event.quotes.get(&leg.instrument_id) else {
                return RawExecution {
                    fills: vec![],
                    reason: Some(format!("missing quote for {}", leg.instrument_id)),
                };
            };
            let source = quote.source_id.to_ascii_lowercase();
            if fact.source_id != quote.source_id
                || source.contains("pchip")
                || source.contains("synthetic")
                || source.contains("interpolat")
            {
                return RawExecution {
                    fills: vec![],
                    reason: Some(format!(
                        "execution source is not verified observed for {}",
                        leg.instrument_id
                    )),
                };
            }
            let Some(capacity) = self.capacity(leg.side, fact) else {
                return RawExecution {
                    fills: vec![],
                    reason: Some(format!(
                        "missing capacity authority for {}",
                        leg.instrument_id
                    )),
                };
            };
            if capacity < leg.quantity && intent.atomic {
                return RawExecution {
                    fills: vec![],
                    reason: Some(format!(
                        "atomic insufficient capacity for {}",
                        leg.instrument_id
                    )),
                };
            }
            let quantity = capacity.min(leg.quantity);
            if quantity == 0 {
                return RawExecution {
                    fills: vec![],
                    reason: Some(format!(
                        "zero executable capacity for {}",
                        leg.instrument_id
                    )),
                };
            }
            let price = match self.slipped_price(leg, fact.tick_size_micro) {
                Ok(value) => value,
                Err(reason) => {
                    return RawExecution {
                        fills: vec![],
                        reason: Some(reason),
                    };
                }
            };
            fills.push(RawFill {
                instrument_id: leg.instrument_id.clone(),
                side: leg.side,
                quantity,
                price,
            });
        }
        RawExecution {
            fills,
            reason: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use backtest_contracts::{CONTRACT_VERSION, IntentAction, IntentLeg, MarketQuote, QuoteUse};
    use std::collections::BTreeSet;

    fn event(ask_size: Option<u64>) -> SealedEvent {
        SealedEvent {
            schema_version: CONTRACT_VERSION.into(),
            event_id: "e".into(),
            sequence: 0,
            decision_at_ns: 1,
            available_at_ns: 1,
            sealed_at_ns: 1,
            quotes: BTreeMap::from([(
                "x".into(),
                MarketQuote {
                    instrument_id: "x".into(),
                    bid: Some(Money(99_000_000)),
                    ask: Some(Money(100_000_000)),
                    mark: None,
                    observed_at_ns: 1,
                    available_at_ns: 1,
                    source_id: "fixture".into(),
                    allowed_uses: BTreeSet::from([QuoteUse::Execution]),
                },
            )]),
            margin_facts: BTreeMap::new(),
            research_payload: serde_json::json!({"execution_liquidity":{"x":{
                "source_kind":"observed","source_id":"fixture","bid_size":10,"ask_size":ask_size,
                "bar_volume":20,"tick_size_micro":50000
            }}}),
        }
    }

    fn intent(atomic: bool) -> TradeIntent {
        TradeIntent {
            schema_version: CONTRACT_VERSION.into(),
            intent_id: "i".into(),
            decision_id: "d".into(),
            strategy_position_id: "p".into(),
            basket_key: "x".into(),
            action: IntentAction::Open,
            atomic,
            legs: vec![IntentLeg {
                instrument_id: "x".into(),
                side: Side::Buy,
                quantity: 10,
                limit_price: None,
            }],
            lineage: BTreeMap::new(),
        }
    }

    fn priced() -> Vec<PricedLeg> {
        vec![PricedLeg {
            instrument_id: "x".into(),
            side: Side::Buy,
            quantity: 10,
            price: Money(100_000_000),
        }]
    }

    #[test]
    fn top_of_book_rejects_atomic_insufficient_size() {
        let mut model = LiquidityExecution::new(ExecutionConfig {
            capacity_mode: CapacityMode::TopOfBook,
            slippage_bps: 0,
            slippage_ticks: 0,
            volume_participation_bps: 0,
        })
        .unwrap();
        let result = model.execute(&event(Some(5)), &intent(true), &priced());
        assert!(result.fills.is_empty());
        assert_eq!(
            result.reason.as_deref(),
            Some("atomic insufficient capacity for x")
        );
    }

    #[test]
    fn applies_conservative_slippage_once() {
        let mut model = LiquidityExecution::new(ExecutionConfig {
            capacity_mode: CapacityMode::UnlimitedQuote,
            slippage_bps: 10,
            slippage_ticks: 1,
            volume_participation_bps: 0,
        })
        .unwrap();
        let result = model.execute(&event(None), &intent(true), &priced());
        assert_eq!(result.fills[0].price, Money(100_100_000));
        assert_eq!(result.fills[0].quantity, 10);
    }

    #[test]
    fn volume_lane_is_explicit_participation_sensitivity() {
        let mut model = LiquidityExecution::new(ExecutionConfig {
            capacity_mode: CapacityMode::BarVolumeSensitivity,
            slippage_bps: 0,
            slippage_ticks: 0,
            volume_participation_bps: 2_500,
        })
        .unwrap();
        let result = model.execute(&event(None), &intent(true), &priced());
        assert!(result.fills.is_empty());
        assert!(result.reason.unwrap().contains("insufficient capacity"));
    }

    #[test]
    fn pchip_cannot_execute_even_if_quote_use_is_mislabeled() {
        let mut input = event(Some(10));
        input.quotes.get_mut("x").unwrap().source_id = "pchip_surface".into();
        input.research_payload["execution_liquidity"]["x"]["source_id"] =
            serde_json::json!("pchip_surface");
        let mut model = LiquidityExecution::new(ExecutionConfig {
            capacity_mode: CapacityMode::TopOfBook,
            slippage_bps: 0,
            slippage_ticks: 0,
            volume_participation_bps: 0,
        })
        .unwrap();
        let result = model.execute(&input, &intent(true), &priced());
        assert!(result.fills.is_empty());
        assert!(result.reason.unwrap().contains("not verified observed"));
    }
}
