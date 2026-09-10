use std::collections::BTreeMap;

use backtest_contracts::{
    AccountState, CONTRACT_VERSION, EngineFeedback, Money, ResearchRequest, SealedEvent,
};
use backtest_engine::DecisionRunner;
use backtest_research_process::{ProcessRunner, ProcessSpec};
use serde_json::{Value, json};

fn request(sequence: u64) -> ResearchRequest {
    ResearchRequest {
        schema_version: CONTRACT_VERSION.to_owned(),
        input: SealedEvent {
            schema_version: CONTRACT_VERSION.to_owned(),
            event_id: format!("event-{sequence}"),
            sequence,
            decision_at_ns: 30,
            available_at_ns: 20,
            sealed_at_ns: 25,
            quotes: BTreeMap::new(),
            margin_facts: BTreeMap::new(),
            research_payload: json!({"sequence": sequence}),
        },
        state: Value::Object(serde_json::Map::new()),
        feedback: EngineFeedback {
            sequence: sequence.saturating_sub(1),
            outcomes: Vec::new(),
            account: AccountState {
                cash: Money(100),
                reserved_margin: Money::ZERO,
                realized_pnl: Money::ZERO,
                unrealized_pnl: Money::ZERO,
                fees_paid: Money::ZERO,
                equity: Money(100),
                positions: Vec::new(),
            },
            blockers: Vec::new(),
            margin: None,
        },
        sequence,
        feedback_context_hash: "context".to_owned(),
        feedback_feature_hash: "features".to_owned(),
    }
}

#[test]
fn one_process_handles_multiple_ordered_requests() {
    let mut runner = ProcessRunner::spawn(&ProcessSpec {
        program: env!("CARGO_BIN_EXE_jsonl-fixture").to_owned(),
        args: Vec::new(),
    })
    .unwrap();
    let first = runner.decide(&request(1)).unwrap();
    let second = runner.decide(&request(2)).unwrap();
    assert_eq!(first.runner_id, "jsonl-contract-fixture");
    assert_eq!(first.state, json!({"sequence": 1}));
    assert_eq!(second.state, json!({"sequence": 2}));
    runner.shutdown().unwrap();
}
