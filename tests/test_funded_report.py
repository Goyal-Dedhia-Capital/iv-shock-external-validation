import json
from pathlib import Path

from ivshock_validation.funded_report import summarize_steps


def test_report_matches_actions_to_outcomes_and_computes_distribution(tmp_path: Path) -> None:
    steps = tmp_path / "steps.jsonl"
    summary = tmp_path / "summary.json"
    action_base = {
        "strategy_position_id": "p",
        "lineage": {"book": "H5_F2", "source_event_id": "iv-shock-source|2025-01-02|1|x"},
    }
    records = []
    for intent_id, action, side, price, fee in [
        ("open", "OPEN", "BUY", 100_000_000, 10_000),
        ("close", "CLOSE", "SELL", 110_000_000, 20_000),
    ]:
        trade_action = {
            **action_base,
            "intent_id": intent_id,
            "decision_id": "d",
            "action": action,
        }
        records.append(
            {
                "response": {"actions": [trade_action]},
                "lifecycle": {
                    "outcomes": [
                        {
                            "intent_id": intent_id,
                            "strategy_position_id": "p",
                            "status": "FILLED",
                            "fills": [
                                {"side": side, "price": price, "quantity": 1, "fee": fee}
                            ],
                            "reason": None,
                        }
                    ]
                },
            }
        )
    steps.write_text("\n".join(json.dumps(record) for record in records) + "\n")
    summary.write_text(json.dumps({"complete": True}))
    result = summarize_steps(steps, summary)
    family = result["families"]["H5_F2"]
    assert family["completed_trades"] == 1
    assert family["gross"]["median_micro"] == 10_000_000
    assert family["net"]["median_micro"] == 9_970_000
    assert family["net"]["win_rate"] == 1.0
    assert family["fees_total_micro"] == 30_000


def test_report_retains_rejected_and_unresolved_separately(tmp_path: Path) -> None:
    steps = tmp_path / "steps.jsonl"
    summary = tmp_path / "summary.json"
    action = {
        "intent_id": "open",
        "decision_id": "d",
        "strategy_position_id": "p",
        "action": "OPEN",
        "lineage": {"book": "H3_F1", "source_event_id": "iv-shock-source|2025-01-02|1|x"},
    }
    record = {
        "response": {"actions": [action]},
        "lifecycle": {
            "outcomes": [
                {
                    "intent_id": "open",
                    "strategy_position_id": "p",
                    "status": "REJECTED",
                    "fills": [],
                    "reason": "missing authoritative opening margin",
                }
            ]
        },
    }
    steps.write_text(json.dumps(record) + "\n")
    summary.write_text(json.dumps({"complete": True}))
    result = summarize_steps(steps, summary)
    family = result["families"]["H3_F1"]
    assert family["completed_trades"] == 0
    assert family["lifecycle_counts"]["open_rejected"] == 1
    assert family["rejection_reasons"]["missing authoritative opening margin"] == 1
