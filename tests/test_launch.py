import json

import pytest
from ivshock_validation.launch import (
    load_bound_json,
    load_validated_source_contract,
    run_bundle_hash,
)


def test_pending_calendar_fails_closed(tmp_path):
    path = tmp_path / "calendar.json"
    path.write_text(json.dumps({"status": "PENDING"}))
    with pytest.raises(ValueError, match="PENDING"):
        load_bound_json(path, kind="exchange calendar")


def test_ready_calendar_is_hashed_and_cannot_use_quotes_for_bounds(tmp_path):
    path = tmp_path / "calendar.json"
    path.write_text(
        json.dumps(
            {"status": "READY", "observed_quotes_may_define_session_bounds": False}
        )
    )
    value, digest = load_bound_json(path, kind="exchange calendar")
    assert value["status"] == "READY"
    assert len(digest) == 64


def test_source_contract_rejects_nested_pending(tmp_path):
    path = tmp_path / "source.json"
    path.write_text(json.dumps({"query": {"identity": "PENDING"}}))
    with pytest.raises(ValueError, match=r"\$\.query\.identity"):
        load_bound_json(path, kind="source contract")


def test_run_bundle_hash_binds_every_identity():
    baseline = run_bundle_hash("revision", "calendar", "source")
    assert baseline != run_bundle_hash("revision-2", "calendar", "source")
    assert baseline != run_bundle_hash("revision", "calendar-2", "source")
    assert baseline != run_bundle_hash("revision", "calendar", "source-2")


def test_strategy_launch_rejects_nonpending_but_incomplete_source(tmp_path):
    path = tmp_path / "source.json"
    path.write_text(json.dumps({"contract_version": 1, "dataset_id": "incomplete"}))
    with pytest.raises(ValueError, match="unresolved source-contract fields"):
        load_validated_source_contract(path, stage="research")
