use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const CONTRACT_VERSION: &str = "gdc.generic-backtest.v1";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Money(pub i64);

impl Money {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn checked_add(self, other: Self) -> Option<Self> {
        match self.0.checked_add(other.0) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    #[must_use]
    pub const fn checked_sub(self, other: Self) -> Option<Self> {
        match self.0.checked_sub(other.0) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum QuoteUse {
    Execution,
    Accounting,
    Margin,
    Research,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketQuote {
    pub instrument_id: String,
    pub bid: Option<Money>,
    pub ask: Option<Money>,
    pub mark: Option<Money>,
    pub observed_at_ns: i64,
    pub available_at_ns: i64,
    pub source_id: String,
    pub allowed_uses: BTreeSet<QuoteUse>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarginFact {
    pub fact_id: String,
    pub basket_key: String,
    pub required: Money,
    pub observed_at_ns: i64,
    pub available_at_ns: i64,
    pub source_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedEvent {
    pub schema_version: String,
    pub event_id: String,
    pub sequence: u64,
    pub decision_at_ns: i64,
    pub available_at_ns: i64,
    pub sealed_at_ns: i64,
    pub quotes: BTreeMap<String, MarketQuote>,
    pub margin_facts: BTreeMap<String, MarginFact>,
    #[serde(default)]
    pub research_payload: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum IntentAction {
    Open,
    Close,
    Reduce,
    Flatten,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntentLeg {
    pub instrument_id: String,
    pub side: Side,
    pub quantity: u64,
    pub limit_price: Option<Money>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TradeIntent {
    pub schema_version: String,
    pub intent_id: String,
    pub decision_id: String,
    pub strategy_position_id: String,
    pub basket_key: String,
    pub action: IntentAction,
    pub atomic: bool,
    pub legs: Vec<IntentLeg>,
    #[serde(default)]
    pub lineage: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OutcomeStatus {
    Filled,
    PartiallyFilled,
    Rejected,
    Deferred,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fill {
    pub instrument_id: String,
    pub side: Side,
    pub quantity: u64,
    pub price: Money,
    pub fee: Money,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionOutcome {
    pub intent_id: String,
    pub strategy_position_id: String,
    pub status: OutcomeStatus,
    pub fills: Vec<Fill>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PositionView {
    pub strategy_position_id: String,
    pub instrument_id: String,
    pub quantity: i64,
    pub average_price: Money,
    pub mark_price: Money,
    pub realized_pnl: Money,
    pub unrealized_pnl: Money,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountState {
    pub cash: Money,
    pub reserved_margin: Money,
    pub realized_pnl: Money,
    pub unrealized_pnl: Money,
    pub fees_paid: Money,
    pub equity: Money,
    pub positions: Vec<PositionView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineFeedback {
    pub sequence: u64,
    pub outcomes: Vec<ExecutionOutcome>,
    pub account: AccountState,
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResearchRequest {
    pub schema_version: String,
    pub input: SealedEvent,
    pub state: Value,
    pub feedback: EngineFeedback,
    pub sequence: u64,
    pub feedback_context_hash: String,
    pub feedback_feature_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResearchResponse {
    pub schema_version: String,
    pub artifact_consumed: bool,
    pub runner_id: String,
    pub bundle_hash: String,
    pub state: Value,
    #[serde(alias = "intents")]
    pub actions: Vec<TradeIntent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleEvidence {
    pub event_id: String,
    pub sequence: u64,
    pub request_hash: String,
    pub response_hash: String,
    pub state_hash: String,
    pub outcomes: Vec<ExecutionOutcome>,
    pub account: AccountState,
}

#[cfg(test)]
mod tests {
    use super::{CONTRACT_VERSION, IntentAction, Money, Side, TradeIntent};

    #[test]
    fn trade_intent_rejects_unknown_fields() {
        let payload = format!(
            r#"{{"schema_version":"{CONTRACT_VERSION}","intent_id":"i","decision_id":"d","strategy_position_id":"p","basket_key":"b","action":"OPEN","atomic":true,"legs":[{{"instrument_id":"x","side":"BUY","quantity":1,"limit_price":100}}],"lineage":{{}},"unsupported":"forbidden"}}"#
        );
        assert!(serde_json::from_str::<TradeIntent>(&payload).is_err());
    }

    #[test]
    fn contract_enums_are_stable() {
        assert_eq!(
            serde_json::to_string(&IntentAction::Flatten).unwrap(),
            "\"FLATTEN\""
        );
        assert_eq!(serde_json::to_string(&Side::Sell).unwrap(), "\"SELL\"");
        assert_eq!(serde_json::to_string(&Money(42)).unwrap(), "42");
    }
}
