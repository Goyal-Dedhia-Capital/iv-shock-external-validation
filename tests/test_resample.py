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


def test_minute_availability_is_latest_contributing_availability() -> None:
    seconds, _ = prepared()
    ce = seconds.filter(pl.col("option_type") == "CE").head(3)
    delayed = datetime(2026, 9, 8, 9, 17, tzinfo=ZoneInfo("Asia/Kolkata"))
    ce = ce.with_columns(
        pl.when(pl.col("timestamp").dt.second() == 0)
        .then(pl.lit(delayed))
        .otherwise(pl.col("available_at"))
        .alias("available_at")
    )
    minute = resample_one_minute(ce.lazy()).collect().row(0, named=True)
    assert minute["available_at"] == delayed
    assert minute["availability_complete"] is False
    assert minute["minute_complete"] is False


def test_complete_event_cadence_with_late_availability_is_not_sealed() -> None:
    seconds, _ = prepared()
    base = seconds.filter(pl.col("option_type") == "CE").head(1)
    start = datetime(2026, 9, 8, 9, 15, tzinfo=ZoneInfo("Asia/Kolkata"))
    timestamps = [start.replace(second=value) for value in range(60)]
    delayed = [value.replace(minute=25) for value in timestamps]
    full = pl.concat([base] * 60).with_columns(
        pl.Series("timestamp", timestamps),
        pl.Series("available_at", delayed),
    )
    minute = resample_one_minute(full.lazy()).collect().row(0, named=True)
    assert minute["cadence_complete"] is True
    assert minute["availability_complete"] is False
    assert minute["minute_complete"] is False


def test_fractionally_shifted_slots_are_not_complete() -> None:
    seconds, _ = prepared()
    base = seconds.filter(pl.col("option_type") == "CE").head(1)
    start = datetime(2026, 9, 8, 9, 15, 0, 500000, tzinfo=ZoneInfo("Asia/Kolkata"))
    timestamps = [start.replace(second=value) for value in range(60)]
    shifted = pl.concat([base] * 60).with_columns(
        pl.Series("timestamp", timestamps),
        pl.Series("available_at", timestamps),
    )
    minute = resample_one_minute(shifted.lazy()).collect().row(0, named=True)
    assert minute["cadence_complete"] is True
    assert minute["cadence_slots_aligned"] is False
    assert minute["minute_complete"] is False


def test_malformed_ohlc_fails_closed() -> None:
    seconds, minutes = prepared()
    contract = json.loads((ROOT / "tests/fixtures/source_contract.ready.json").read_text())
    broken = seconds.with_columns(pl.lit(200.0).alias("low"))
    with pytest.raises(ValueError, match="malformed OHLC"):
        audit_frames(broken, minutes, contract)


def test_admission_requires_complete_minutes_and_expiry_depth() -> None:
    seconds, _ = prepared()
    base = seconds.filter(pl.col("option_type") == "CE").head(1)
    start = datetime(2026, 9, 8, 9, 15, tzinfo=ZoneInfo("Asia/Kolkata"))
    timestamps = [start.replace(second=value) for value in range(60)]
    expiries = []
    for offset, expiry in enumerate(["2026-09-24", "2026-10-29", "2026-11-26"]):
        rows = pl.concat([base] * 60).with_columns(
            pl.Series("timestamp", timestamps),
            pl.Series("available_at", timestamps),
            pl.lit(datetime.fromisoformat(expiry).date()).alias("expiry"),
            pl.lit(f"NIFTY|{expiry}|25000|CE").alias("contract_id"),
            pl.lit(25000.0 + offset * 50).alias("strike"),
        )
        expiries.append(rows)
    full = pl.concat(expiries)
    minutes = resample_one_minute(full.lazy()).collect()
    contract = json.loads((ROOT / "tests/fixtures/source_contract.ready.json").read_text())
    audit = audit_frames(full, minutes, contract)
    assert audit["admission_status"] == "READY"
    assert audit["admission_blockers"] == []


def test_nonfinite_prices_fail_closed() -> None:
    seconds, minutes = prepared()
    contract = json.loads((ROOT / "tests/fixtures/source_contract.ready.json").read_text())
    broken = seconds.with_columns(pl.lit(float("nan")).alias("close"))
    with pytest.raises(ValueError, match="non-finite canonical numeric"):
        audit_frames(broken, minutes, contract)


def test_rows_outside_declared_coverage_fail_closed() -> None:
    seconds, minutes = prepared()
    contract = json.loads((ROOT / "tests/fixtures/source_contract.ready.json").read_text())
    contract["coverage"]["last_session"] = "2026-09-07"
    with pytest.raises(ValueError, match="outside declared coverage"):
        audit_frames(seconds, minutes, contract)


def test_declared_source_mappings_are_used() -> None:
    contract = json.loads((ROOT / "tests/fixtures/source_contract.ready.json").read_text())
    replacements = {
        "volume": "provider_volume",
        "oi": "provider_oi",
        "iv": "provider_iv",
        "ttm": "provider_ttm",
        "forward": "provider_forward",
    }
    contract["volume"]["column"] = replacements["volume"]
    contract["open_interest"]["column"] = replacements["oi"]
    contract["iv"]["column"] = replacements["iv"]
    contract["iv"]["ttm_column"] = replacements["ttm"]
    contract["underlying"]["forward_column"] = replacements["forward"]
    source = scan_source(ROOT / "tests/fixtures/one_second.csv").rename(replacements)
    normalized = normalize_source(source, contract).collect()
    assert normalized["volume"].drop_nulls().head(1).item() == 2
    assert normalized["open_interest"].head(1).item() == 1000
    assert normalized["calendar_iv"].head(1).item() == 0.20
    assert normalized["calendar_ttm"].head(1).item() == pytest.approx(21 / 365)
    assert normalized["forward"].head(1).item() == 23805


def test_execution_missing_quotes_blocks_admission() -> None:
    seconds, _ = prepared()
    base = seconds.filter(pl.col("option_type") == "CE").head(1)
    start = datetime(2026, 9, 8, 9, 15, tzinfo=ZoneInfo("Asia/Kolkata"))
    timestamps = [start.replace(second=value) for value in range(60)]
    expiries = []
    for offset, expiry in enumerate(["2026-09-24", "2026-10-29", "2026-11-26"]):
        expiries.append(
            pl.concat([base] * 60).with_columns(
                pl.Series("timestamp", timestamps),
                pl.Series("available_at", timestamps),
                pl.lit(datetime.fromisoformat(expiry).date()).alias("expiry"),
                pl.lit(f"NIFTY|{expiry}|25000|CE").alias("contract_id"),
                pl.lit(25000.0 + offset * 50).alias("strike"),
                pl.lit(None, dtype=pl.Float64).alias("bid"),
            )
        )
    full = pl.concat(expiries)
    minutes = resample_one_minute(full.lazy()).collect()
    contract = json.loads((ROOT / "tests/fixtures/source_contract.ready.json").read_text())
    audit = audit_frames(full, minutes, contract, stage="execution")
    assert audit["admission_status"] == "BLOCKED"
    assert "execution_quotes_missing" in audit["admission_blockers"]
