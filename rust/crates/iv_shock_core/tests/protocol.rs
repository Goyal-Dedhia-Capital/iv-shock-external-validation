use backtest_contracts::CONTRACT_VERSION;
use iv_shock_decision::{Detector, JsonlRequest, Runner, RunnerConfig};
use serde_json::json;

#[test]
fn canonical_jsonl_dispatch_and_clock_validation() {
    let mut runner = Runner::default();
    let mut request = json!({
        "schema_version": CONTRACT_VERSION,
        "input": {"schema_version":CONTRACT_VERSION,"event_id":"fixture","sequence":0,
            "decision_at_ns":0,"available_at_ns":0,"sealed_at_ns":0,
            "quotes":{},"margin_facts":{},"research_payload":{"kind":"session_start","session_date":"2024-01-01"}},
        "state":null,"sequence":0,"feedback_context_hash":"fixture","feedback_feature_hash":"fixture",
        "feedback":{"sequence":0,"outcomes":[],"blockers":[],"account":{"cash":0,"reserved_margin":0,
            "realized_pnl":0,"unrealized_pnl":0,"fees_paid":0,"equity":0,"positions":[]}}
    });
    let encoded = runner
        .process_json(&request.to_string())
        .expect("canonical process dispatch");
    let response: backtest_contracts::ResearchResponse =
        serde_json::from_str(&encoded).expect("canonical response shape");
    assert_eq!(response.schema_version, CONTRACT_VERSION);
    assert_eq!(response.state["runner_state"]["next_sequence"], 1);
    request["sequence"] = json!(1);
    request["input"]["sequence"] = json!(1);
    request["input"]["research_payload"] =
        json!({"kind":"minute_packet","session_date":"2024-01-01","ts_minute":1,"events":[]});
    assert!(
        runner.process_json(&request.to_string()).is_err(),
        "source and envelope clocks must match"
    );
    request["input"]["decision_at_ns"] = json!(60_000_000_000_i64);
    request["input"]["available_at_ns"] = json!(60_000_000_000_i64);
    request["input"]["sealed_at_ns"] = json!(60_000_000_000_i64);
    assert!(runner.process_json(&request.to_string()).is_ok());
}

fn event(contract_id: &str, raw_delta: f64, ts: i64) -> serde_json::Value {
    json!({
        "contract_id": contract_id,
        "raw_delta": raw_delta,
        "log_delta": raw_delta / 100.0,
        "residual": raw_delta / 10.0,
        "calibration_sample": true,
        "neighbor_count": 3,
        "group_keys": {"l0": "full", "l1": "side", "l2": "bucket", "l3": "all"},
        "signal_metadata": {"raw_iv_sign": if raw_delta >= 0.0 { 1 } else { -1 }, "inherited_sign": 1 },
        "ts_minute": ts
    })
}

#[allow(clippy::needless_pass_by_value)]
fn request(sequence: u64, input: serde_json::Value, state: serde_json::Value) -> JsonlRequest {
    serde_json::from_value(json!({
        "input": input,
        "state": state,
        "feedback": {},
        "sequence": sequence
    }))
    .expect("request schema")
}

#[test]
fn strict_sequence_and_state_persistence() {
    let mut runner = Runner::default();
    let first = runner
        .process_request(request(
            0,
            json!({"kind":"session_start","session_date":"2024-01-01"}),
            json!(null),
        ))
        .expect("start");
    assert!(first.artifact_consumed);
    assert_eq!(first.matching_sequence, 0);
    let error = runner.process_request(request(
        0,
        json!({"kind":"session_end","session_date":"2024-01-01"}),
        json!(null),
    ));
    assert!(error.is_err(), "reusing a sequence must fail closed");
    let second = runner
        .process_request(request(
            1,
            json!({"kind":"session_end","session_date":"2024-01-01"}),
            first.state,
        ))
        .expect("end");
    assert_eq!(second.matching_sequence, 1);
}

#[test]
fn six_diagnostics_have_causal_missing_reasons() {
    let mut runner = Runner::new(RunnerConfig {
        minimum_support: 1,
        emit_all_diagnostics: true,
        ..RunnerConfig::default()
    });
    let start = runner
        .process_request(request(
            0,
            json!({"kind":"session_start","session_date":"2024-01-01"}),
            json!(null),
        ))
        .expect("start");
    let packet = json!({
        "kind": "minute_packet",
        "session_date": "2024-01-01",
        "ts_minute": 600,
        "events": [event("c", 1.0, 600)]
    });
    let response = runner
        .process_request(request(1, packet, start.state))
        .expect("packet");
    assert_eq!(response.diagnostics[0].detectors.len(), Detector::ALL.len());
    assert!(
        response.diagnostics[0]
            .detectors
            .iter()
            .any(|d| d.reason.is_some())
    );
}

#[test]
fn minute_appends_compact_raw_history_only() {
    let mut runner = Runner::default();
    let start = runner
        .process_request(request(
            0,
            json!({"kind":"session_start","session_date":"2024-01-01"}),
            json!(null),
        ))
        .expect("start");
    runner
        .process_request(request(
            1,
            json!({
                "kind":"minute_packet",
                "session_date":"2024-01-01",
                "ts_minute":600,
                "events":[event("c", 1.0, 600)]
            }),
            start.state,
        ))
        .expect("minute");
    let session = runner
        .state()
        .current_session
        .as_ref()
        .expect("active session");
    assert_eq!(session.raw_by_contract["c"], vec![(600, 1.0)]);
    assert!(session.eligible_raw_history.is_empty());
}

#[test]
fn quiet_gap_uses_strictly_prior_qualified_timestamp() {
    let mut runner = Runner::new(RunnerConfig {
        minimum_support: 1,
        quiet_gap_minutes: 15,
        emit_all_diagnostics: true,
        ..RunnerConfig::default()
    });
    let start = runner
        .process_request(request(
            0,
            json!({"kind":"session_start","session_date":"2024-01-01","scoring":false}),
            json!(null),
        ))
        .expect("start");
    let seeded = runner
        .process_request(request(1, json!({"kind":"minute_packet","session_date":"2024-01-01","ts_minute":600,"events":[event("c",0.0,600)]}), start.state))
        .expect("seed packet");
    let seeded_second = runner
        .process_request(request(2, json!({"kind":"minute_packet","session_date":"2024-01-01","ts_minute":601,"events":[event("c",1.0,601)]}), seeded.state))
        .expect("second seed packet");
    let seeded_end = runner
        .process_request(request(
            3,
            json!({"kind":"session_end","session_date":"2024-01-01"}),
            seeded_second.state,
        ))
        .expect("seed end");
    let current = runner
        .process_request(request(
            4,
            json!({"kind":"session_start","session_date":"2024-01-02"}),
            seeded_end.state,
        ))
        .expect("current start");
    let first = runner
        .process_request(request(5, json!({"kind":"minute_packet","session_date":"2024-01-02","ts_minute":600,"events":[event("c",10.0,600)]}), current.state))
        .expect("first");
    let second = runner
        .process_request(request(6, json!({"kind":"minute_packet","session_date":"2024-01-02","ts_minute":610,"events":[event("c",10.0,610)]}), first.state))
        .expect("second");
    assert!(
        second.diagnostics[0]
            .detectors
            .iter()
            .all(|d| !d.qualified_after_quiet)
    );
    let third = runner
        .process_request(request(7, json!({"kind":"minute_packet","session_date":"2024-01-02","ts_minute":625,"events":[event("c",10.0,625)]}), second.state))
        .expect("third");
    assert!(
        third.diagnostics[0]
            .detectors
            .iter()
            .any(|d| d.qualified_after_quiet)
    );
}

#[test]
fn current_session_is_excluded_from_calibration() {
    let mut runner = Runner::new(RunnerConfig {
        minimum_support: 2,
        emit_all_diagnostics: true,
        ..RunnerConfig::default()
    });
    let start = runner
        .process_request(request(
            0,
            json!({"kind":"session_start","session_date":"2024-01-01"}),
            json!(null),
        ))
        .expect("start");
    let packet = json!({"kind":"minute_packet","session_date":"2024-01-01","ts_minute":600,"events":[event("c",9.0,600)]});
    let response = runner
        .process_request(request(1, packet, start.state))
        .expect("packet");
    assert!(
        response.diagnostics[0]
            .detectors
            .iter()
            .all(|d| d.support == 0)
    );
}

#[test]
fn detector_rejects_direct_intent_bypass() {
    let mut runner = Runner::new(RunnerConfig {
        minimum_support: 1,
        ..RunnerConfig::default()
    });
    let start = runner
        .process_request(request(
            0,
            json!({"kind":"session_start","session_date":"2024-01-01"}),
            json!(null),
        ))
        .expect("start");
    let mut value = event("c", 10.0, 600);
    value["intent"] =
        json!({"detector":"S0","action":"OPEN","side":"BUY","quantity":1,"instrument_id":"c"});
    let packet = json!({"kind":"minute_packet","session_date":"2024-01-01","ts_minute":600,"emit_actions":true,"events":[value]});
    assert!(
        runner
            .process_request(request(1, packet, start.state))
            .is_err()
    );
}

#[test]
fn canonical_research_request_boundary_is_supported() {
    let mut runner = Runner::default();
    let request: backtest_contracts::ResearchRequest = serde_json::from_value(json!({
        "schema_version": CONTRACT_VERSION,
        "input": {
            "schema_version": CONTRACT_VERSION,
            "event_id": "e0",
            "sequence": 0,
            "decision_at_ns": 0,
            "available_at_ns": 0,
            "sealed_at_ns": 0,
            "quotes": {},
            "margin_facts": {},
            "research_payload": {"kind": "session_start", "session_date": "2024-01-01"}
        },
        "state": {},
        "feedback": {
            "sequence": 0,
            "outcomes": [],
            "account": {
                "cash": 0,
                "reserved_margin": 0,
                "realized_pnl": 0,
                "unrealized_pnl": 0,
                "fees_paid": 0,
                "equity": 0,
                "positions": []
            },
            "blockers": []
        },
        "sequence": 0,
        "feedback_context_hash": "context",
        "feedback_feature_hash": "features"
    }))
    .expect("canonical request schema");
    let response = runner
        .process_canonical_request(request)
        .expect("canonical boundary");
    assert!(response.artifact_consumed);
    assert_eq!(response.schema_version, CONTRACT_VERSION);
    assert!(response.state.get("runner_state").is_some());
}

#[test]
fn minute_clock_and_duplicate_batch_fail_without_mutation() {
    let mut runner = Runner::default();
    runner
        .process_request(request(
            0,
            json!({"kind":"session_start","session_date":"2024-01-01"}),
            json!(null),
        ))
        .unwrap();
    let packet = |t, events| json!({"kind":"minute_packet","session_date":"2024-01-01","ts_minute":t,"events":events});
    runner
        .process_request(request(
            1,
            packet(600, json!([event("a", 1., 600)])),
            json!(null),
        ))
        .unwrap();
    let before = serde_json::to_value(runner.state()).unwrap();
    for t in [500, 600] {
        assert!(
            runner
                .process_request(request(2, packet(t, json!([])), json!(null)))
                .is_err()
        );
        assert_eq!(serde_json::to_value(runner.state()).unwrap(), before);
    }
    assert!(
        runner
            .process_request(request(
                2,
                packet(601, json!([event("b", 2., 601), event("b", 2., 601)])),
                json!(null)
            ))
            .is_err()
    );
    assert_eq!(serde_json::to_value(runner.state()).unwrap(), before);
    let path = std::env::temp_dir().join(format!("h5-clock-{}.json", std::process::id()));
    runner.write_checkpoint(path.to_str().unwrap()).unwrap();
    let mut restored = Runner::default();
    restored.restore_checkpoint(path.to_str().unwrap()).unwrap();
    assert_eq!(
        restored
            .state()
            .current_session
            .as_ref()
            .unwrap()
            .last_minute,
        Some(600)
    );
    std::fs::remove_file(path).unwrap();
    runner
        .process_request(request(2, packet(601, json!([])), json!(null)))
        .unwrap();
}

#[test]
fn checkpoint_failure_and_restore_are_transactional() {
    let mut runner = Runner::default();
    runner
        .process_request(request(
            0,
            json!({"kind":"session_start","session_date":"2024-01-01"}),
            json!(null),
        ))
        .unwrap();
    let ended = runner
        .process_request(request(
            1,
            json!({"kind":"session_end","session_date":"2024-01-01"}),
            json!(null),
        ))
        .unwrap();
    assert_eq!(ended.state["last_completed_session"], "2024-01-01");
    let state = serde_json::to_value(runner.state()).unwrap();
    for packet in [
        json!({"kind":"checkpoint"}),
        json!({"kind":"checkpoint","path":"/tmp/h5-missing-directory-transactional/checkpoint.json"}),
        json!({"kind":"restore","path":"unused"}),
    ] {
        assert!(
            runner
                .process_request(request(2, packet, json!(null)))
                .is_err()
        );
        assert_eq!(serde_json::to_value(runner.state()).unwrap(), state);
    }
    let mut fresh = Runner::default();
    assert!(
        fresh
            .process_request(request(
                2,
                json!({"kind":"session_start","session_date":"2024-01-02"}),
                ended.state
            ))
            .is_err(),
        "metadata cannot recreate calibration history"
    );
}
