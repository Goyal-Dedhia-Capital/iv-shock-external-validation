from datetime import date, datetime
from zoneinfo import ZoneInfo

import polars as pl
import pytest
from ivshock_validation.asjad_adapter import AdapterConfig, adapt_asjad_frames


def options() -> pl.DataFrame:
    return pl.DataFrame(
        {
            "timestamp": ["2026-09-08 09:15:00", "2026-09-08 09:15:00"],
            "ticker": ["NIFTY", "NIFTY"],
            "expiry_date": ["2026-09-29", "2026-09-29"],
            "strike_price": [24000.0, 24000.0],
            "option_type": ["ce", "pe"],
            "open": [100.0, 90.0],
            "high": [101.0, 91.0],
            "low": [99.0, 89.0],
            "close": [100.5, 90.5],
            "volume": [2, -1],
            "oi": [1000, 1200],
            "buy_price": [100.0, 90.0],
            "sell_price": [101.0, 91.0],
            "forward": [24010.0, 24010.0],
            "ttm": [21.0, 21.0],
            "iv": [0.2, 0.21],
            "delta": [0.5, -0.5],
            "gamma": [0.001, 0.001],
            "theta": [-1.0, -1.0],
            "vega": [10.0, 10.0],
        }
    )


def spot() -> pl.DataFrame:
    return pl.DataFrame(
        {
            "timestamp": ["2026-09-08 09:15:00"],
            "ticker": ["NIFTY"],
            "spot": [24005.0],
            "available_at": ["2026-09-08 09:15:01"],
        }
    )


def lots() -> pl.DataFrame:
    return pl.DataFrame(
        {"session_date": ["2026-09-08"], "ticker": ["NIFTY"], "lot_size": [25]}
    )


def test_exact_join_adds_authoritative_spot_lot_and_combined_availability() -> None:
    result = adapt_asjad_frames(
        options().lazy(),
        spot=spot().lazy(),
        lots=lots().lazy(),
        config=AdapterConfig(options_availability_equals_timestamp=True),
    )
    assert result.height == 2
    assert result["option_type"].to_list() == ["CE", "PE"]
    assert result["spot"].to_list() == [24005.0, 24005.0]
    assert result["lot_size"].to_list() == [25, 25]
    assert result["session_date"].to_list() == [date(2026, 9, 8)] * 2
    assert result["timestamp"].dtype == pl.Datetime("us", "Asia/Kolkata")
    assert result["available_at"].to_list() == [
        datetime(2026, 9, 8, 9, 15, 1, tzinfo=ZoneInfo("Asia/Kolkata"))
    ] * 2


def test_adapter_never_substitutes_forward_for_missing_spot() -> None:
    with pytest.raises(ValueError, match="forward cannot substitute"):
        adapt_asjad_frames(
            options().lazy(),
            lots=lots().lazy(),
            config=AdapterConfig(options_availability_equals_timestamp=True),
        )


def test_exact_spot_join_does_not_asof_fill() -> None:
    later = spot().with_columns(pl.lit("2026-09-08 09:15:01").alias("timestamp"))
    with pytest.raises(ValueError, match="spot is missing or invalid"):
        adapt_asjad_frames(
            options().lazy(),
            spot=later.lazy(),
            lots=lots().lazy(),
            config=AdapterConfig(options_availability_equals_timestamp=True),
        )


def test_availability_requires_explicit_attestation() -> None:
    with pytest.raises(ValueError, match="attestation is required"):
        adapt_asjad_frames(options().lazy(), spot=spot().lazy(), lots=lots().lazy())


def test_duplicate_option_keys_fail_closed() -> None:
    duplicated = pl.concat([options(), options().head(1)])
    with pytest.raises(ValueError, match="duplicate economic row keys"):
        adapt_asjad_frames(
            duplicated.lazy(),
            spot=spot().lazy(),
            lots=lots().lazy(),
            config=AdapterConfig(options_availability_equals_timestamp=True),
        )


def test_each_source_availability_must_not_precede_observation() -> None:
    early_spot = spot().with_columns(
        pl.lit("2026-09-08 09:14:59").alias("available_at")
    )
    with pytest.raises(ValueError, match="availability precedes"):
        adapt_asjad_frames(
            options().lazy(),
            spot=early_spot.lazy(),
            lots=lots().lazy(),
            config=AdapterConfig(options_availability_equals_timestamp=True),
        )

    early_options = options().with_columns(
        pl.lit("2026-09-08 09:14:59").alias("available_at")
    )
    with pytest.raises(ValueError, match="availability precedes"):
        adapt_asjad_frames(
            early_options.lazy(),
            spot=spot().lazy(),
            lots=lots().lazy(),
            config=AdapterConfig(),
        )
