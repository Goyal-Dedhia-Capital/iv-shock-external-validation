import json
from pathlib import Path

ROOT = Path(__file__).parents[1]


def load(name: str) -> dict:
    return json.loads((ROOT / "variants" / name).read_text())


def test_h3_identity_and_exit_policy_are_causal() -> None:
    registry = load("h3_families.json")
    assert "exit_minute" not in registry["sequential"]["candidate_dedup_key"]
    assert registry["timing"]["primary_exit_policy"] == "scheduled_exact_same_session_close"
    assert registry["timing"]["sensitivity_exit_policy"] == "next_available_same_session_close"
    assert registry["sequential"]["scheduling_order"].index("sign_pooled_non_overlap") < registry[
        "sequential"
    ]["scheduling_order"].index("entry_availability")


def test_h5_moneyness_and_selection_are_event_time_only() -> None:
    registry = load("h5_families.json")
    assert registry["coverage"]["recipient_log_moneyness_formula"] == (
        "ln(recipient_strike/spot_at_source_event)"
    )
    assert registry["recipient_selection"]["endpoint_availability_may_affect_selection"] is False
    assert registry["views"]["primary_exit_policy"] == "scheduled_exact_same_session_close"


def test_unfinished_detectors_and_final_candidate_status_are_explicit() -> None:
    detectors = load("detectors.json")["detectors"]
    assert detectors[0]["external_validation_gate"] == "READY_FOR_SYNTHETIC_IMPLEMENTATION_TESTS"
    assert all(item["external_validation_gate"].startswith("BLOCKED_") for item in detectors[1:])
    candidate = load("final_candidate_policy.json")
    assert candidate["execution_sensitivity"]["funded_claim_allowed"] is False
    assert candidate["basket_rules_status"].startswith("executable_rust_policy_")
    assert candidate["remaining_external_inputs"]
