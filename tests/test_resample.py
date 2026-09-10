import json
from datetime import datetime
from pathlib import Path
from zoneinfo import ZoneInfo

import polars as pl
import pytest
from ivshock_validation.audit import audit_frames
from ivshock_validation.contracts import validate_source_contract
from ivshock_validation.io import scan_source
from ivshock_validation.normalize import normalize_source
from ivshock_validation.resample import resample_one_minute

ROOT = Path(__file__).parents[1]


def prepared() -> tuple[pl.DataFrame, pl.DataFrame]:
    contract = json.loads((ROOT / "tests/fixtures/source_contract.ready.json").read_text())
    validate_source_contract(contract, stage="execution")
    seconds = normalize_source(
        scan_source(ROOT / "tests/fixtures/one_second.csv"), contract
    ).collect()
    minutes = resample_one_minute(seconds.lazy()).collect()
    return seconds, minutes


def test_causal_minute_ohlc_and_last_observation() -> None:
    seconds, minutes = prepared()
    ce = minutes.filter(
        (pl.col("option_type") == "CE")
        & (pl.col("minute") == datetime(2026, 9, 8, 9, 15, tzinfo=ZoneInfo("Asia/Kolkata")))
    ).row(0, named=True)
    assert ce["open"] == 100
    assert ce["high"] == 105
    assert ce["low"] == 98
    assert ce["close"] == 99
    assert ce["bid"] == 98.5
    assert ce["ask"] == 99.5
    assert ce["volume"] == 5
    assert ce["open_interest"] == 1020
    assert ce["observed_seconds"] == 3
    assert ce["minute_complete"] is False
    assert seconds.height == 6


def test_next_minute_does_not_leak_backwards() -> None:
    _, minutes = prepared()
    ce = minutes.filter(pl.col("option_type") == "CE").sort("minute")
    assert ce["close"].to_list() == [99.0, 98.0]
    assert ce["volume"].to_list() == [5.0, 4.0]


def test_audit_conserves_unique_source_keys() -> None:
    seconds, minutes = prepared()
    contract = json.loads((ROOT / "tests/fixtures/source_contract.ready.json").read_text())
    audit = audit_frames(seconds, minutes, contract)
    assert audit["normalized_rows"] == 6
    assert audit["minute_rows"] == 3
    assert audit["complete_minutes"] == 0
    assert audit["partial_minutes"] == 3
    assert audit["duplicate_key_rows"] == 0
    assert audit["contracts"] == 2
    assert audit["within_contract_cadence_gaps"] == 2


def test_only_full_source_minute_is_complete() -> None:
    seconds, _ = prepared()
    base = seconds.filter(pl.col("option_type") == "CE").head(1)
    start = datetime(2026, 9, 8, 9, 15, tzinfo=ZoneInfo("Asia/Kolkata"))
    timestamps = [start.replace(second=value) for value in range(60)]
    full = pl.concat([base] * 60).with_columns(
        pl.Series("timestamp", timestamps),
        pl.Series("available_at", timestamps),
    )
    minute = resample_one_minute(full.lazy()).collect().row(0, named=True)
    assert minute["observed_seconds"] == 60
    assert minute["minute_complete"] is True


def test_rows_outside_declared_session_fail_closed() -> None:
    seconds, minutes = prepared()
    contract = json.loads((ROOT / "tests/fixtures/source_contract.ready.json").read_text())
    contract["timestamp"]["session_close"] = "09:14:59"
    with pytest.raises(ValueError, match="outside declared session"):
        audit_frames(seconds, minutes, contract)
