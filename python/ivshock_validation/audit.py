"""Compact, non-sensitive preparation audit."""

from __future__ import annotations

from datetime import date, time
from typing import Any

import polars as pl

SOURCE_KEY = ["timestamp", "ticker", "expiry", "strike", "option_type"]
IDENTITY_COLUMNS = [
    "timestamp",
    "available_at",
    "ticker",
    "contract_id",
    "expiry",
    "strike",
    "option_type",
]
POSITIVE_COLUMNS = ["strike", "spot", "open", "high", "low", "close", "bid", "ask"]
FINITE_COLUMNS = [
    "strike",
    "spot",
    "forward",
    "open",
    "high",
    "low",
    "close",
    "bid",
    "ask",
    "volume",
    "open_interest",
    "calendar_ttm",
    "calendar_iv",
    "delta",
    "gamma",
    "theta",
    "vega",
]
ECONOMIC_CONTRACT = ["ticker", "expiry", "strike", "option_type"]


def audit_frames(
    seconds: pl.DataFrame,
    minutes: pl.DataFrame,
    contract: dict[str, Any],
    *,
    stage: str = "research",
) -> dict[str, object]:
    null_identity = sum(seconds[name].null_count() for name in IDENTITY_COLUMNS)
    if null_identity:
        raise ValueError(f"null canonical identity values: {null_identity}")
    duplicate_rows = seconds.height - seconds.unique(SOURCE_KEY).height
    if duplicate_rows:
        raise ValueError(f"duplicate normalized source keys: {duplicate_rows}")
    if seconds.filter(~pl.col("option_type").is_in(["CE", "PE"])).height:
        raise ValueError("option_type must be CE or PE")
    if seconds.filter(pl.col("ticker") != contract["symbol"]).height:
        raise ValueError("canonical ticker does not match source-contract symbol")
    null_ohlc = sum(seconds[name].null_count() for name in ("open", "high", "low", "close"))
    if null_ohlc:
        raise ValueError(f"null one-second OHLC values: {null_ohlc}")
    non_finite = sum(
        seconds.filter(pl.col(name).is_not_null() & ~pl.col(name).is_finite()).height
        for name in FINITE_COLUMNS
    )
    if non_finite:
        raise ValueError(f"non-finite canonical numeric values: {non_finite}")
    non_positive = sum(
        seconds.filter(pl.col(name).is_not_null() & (pl.col(name) <= 0)).height
        for name in POSITIVE_COLUMNS
    )
    if non_positive:
        raise ValueError(f"non-positive canonical price values: {non_positive}")
    malformed_ohlc = seconds.filter(
        (pl.col("low") > pl.min_horizontal("open", "close", "high"))
        | (pl.col("high") < pl.max_horizontal("open", "close", "low"))
    ).height
    if malformed_ohlc:
        raise ValueError(f"malformed OHLC rows: {malformed_ohlc}")
    negative_volume = seconds.filter(pl.col("volume").is_not_null() & (pl.col("volume") < 0)).height
    if negative_volume:
        raise ValueError(f"negative volume rows after normalization: {negative_volume}")
    negative_oi = seconds.filter(
        pl.col("open_interest").is_not_null() & (pl.col("open_interest") < 0)
    ).height
    if negative_oi:
        raise ValueError(f"negative open-interest rows: {negative_oi}")
    contract_id_collisions = (
        seconds.group_by("contract_id")
        .agg(pl.struct(ECONOMIC_CONTRACT).n_unique().alias("economic_contracts"))
        .filter(pl.col("economic_contracts") != 1)
        .height
    )
    economic_id_collisions = (
        seconds.group_by(ECONOMIC_CONTRACT)
        .agg(pl.col("contract_id").n_unique().alias("contract_ids"))
        .filter(pl.col("contract_ids") != 1)
        .height
    )
    if contract_id_collisions or economic_id_collisions:
        raise ValueError(
            "contract identity collisions: "
            f"id_to_economics={contract_id_collisions}, economics_to_id={economic_id_collisions}"
        )
    crossed = seconds.filter(
        pl.col("bid").is_not_null() & pl.col("ask").is_not_null() & (pl.col("bid") > pl.col("ask"))
    ).height
    if crossed:
        raise ValueError(f"crossed canonical quotes: {crossed}")
    unavailable = seconds.filter(pl.col("available_at") < pl.col("timestamp")).height
    if unavailable:
        raise ValueError(f"availability precedes source timestamp: {unavailable}")
    timestamp_zone = seconds.schema["timestamp"].time_zone
    available_zone = seconds.schema["available_at"].time_zone
    declared_zone = contract["timestamp"]["timezone"]
    if timestamp_zone != declared_zone or available_zone != declared_zone:
        raise ValueError(
            "canonical timestamp timezone mismatch: "
            f"timestamp={timestamp_zone}, available_at={available_zone}, declared={declared_zone}"
        )

    open_time = time.fromisoformat(contract["timestamp"]["session_open"])
    close_time = time.fromisoformat(contract["timestamp"]["session_close"])
    outside_session = seconds.filter(
        (pl.col("timestamp").dt.time() < open_time) | (pl.col("timestamp").dt.time() > close_time)
    ).height
    if outside_session:
        raise ValueError(f"source rows outside declared session: {outside_session}")
    first_session = date.fromisoformat(contract["coverage"]["first_session"])
    last_session = date.fromisoformat(contract["coverage"]["last_session"])
    outside_coverage = seconds.filter(
        (pl.col("session_date") < first_session) | (pl.col("session_date") > last_session)
    ).height
    if outside_coverage:
        raise ValueError(f"source rows outside declared coverage dates: {outside_coverage}")
    expired_contracts = seconds.filter(pl.col("expiry") < pl.col("session_date")).height
    if expired_contracts:
        raise ValueError(f"expired contract rows: {expired_contracts}")

    out_of_order = (
        seconds.with_columns(
            pl.col("timestamp")
            .diff()
            .over(["session_date", "contract_id"])
            .alias("_input_order_delta")
        )
        .filter(pl.col("_input_order_delta").dt.total_seconds() < 0)
        .height
    )

    cadence = contract["timestamp"]["expected_cadence_seconds"]
    gaps = (
        seconds.sort(["session_date", "contract_id", "timestamp"])
        .with_columns(
            pl.col("timestamp")
            .diff()
            .over(["session_date", "contract_id"])
            .dt.total_seconds()
            .alias("_gap_seconds")
        )
        .filter(pl.col("_gap_seconds") > cadence)
        .height
    )

    expiry_counts = seconds.group_by("session_date").agg(pl.col("expiry").n_unique())

    first_timestamp = seconds["timestamp"].min()
    last_timestamp = seconds["timestamp"].max()
    warnings = []
    if minutes.filter(~pl.col("minute_complete")).height:
        warnings.append("partial_minutes_present")
    if gaps:
        warnings.append("within_contract_cadence_gaps")
    if out_of_order:
        warnings.append("input_out_of_order")
    if seconds["calendar_iv"].null_count():
        warnings.append("missing_iv")
    invalid_iv = seconds.filter(
        pl.col("calendar_iv").is_not_null() & (pl.col("calendar_iv") <= 0)
    ).height
    if invalid_iv:
        warnings.append("invalid_iv")
    if seconds["spot"].null_count():
        warnings.append("missing_spot")
    if seconds["bid"].null_count() or seconds["ask"].null_count():
        warnings.append("missing_quotes")
    if seconds["forward"].null_count():
        warnings.append("missing_forward")
    invalid_forward = seconds.filter(
        pl.col("forward").is_not_null() & (pl.col("forward") <= 0)
    ).height
    if invalid_forward:
        warnings.append("invalid_forward")
    invalid_ttm = seconds.filter(
        pl.col("calendar_ttm").is_not_null() & (pl.col("calendar_ttm") <= 0)
    ).height
    if invalid_ttm:
        warnings.append("invalid_ttm")
    if seconds["volume"].null_count():
        warnings.append("missing_volume")
    if seconds["open_interest"].null_count():
        warnings.append("missing_open_interest")
    if any(seconds[name].null_count() for name in ("delta", "gamma", "theta", "vega")):
        warnings.append("missing_greeks")

    blockers = []
    if seconds.height == 0:
        blockers.append("no_normalized_rows")
    if minutes.filter(pl.col("minute_complete")).height == 0:
        blockers.append("no_complete_minutes")
    minimum_expiries = expiry_counts["expiry"].min()
    required_expiries = contract["coverage"]["minimum_expiries_per_session"]
    if minimum_expiries is None or minimum_expiries < required_expiries:
        blockers.append("expiry_inventory_below_declared_minimum")
    formation_eligible = minutes.filter(
        pl.col("minute_complete")
        & (pl.col("spot") > 0)
        & (pl.col("forward") > 0)
        & (pl.col("calendar_ttm") > 0)
        & (pl.col("calendar_iv") > 0)
    ).height
    if formation_eligible == 0:
        blockers.append("no_formation_eligible_rows")
    if stage == "execution":
        if seconds["bid"].null_count() or seconds["ask"].null_count():
            blockers.append("execution_quotes_missing")
        invalid_lots = seconds.filter(
            pl.col("lot_size").is_null() | (pl.col("lot_size") <= 0)
        ).height
        if invalid_lots:
            blockers.append("execution_lot_size_missing_or_invalid")
    admission_status = "BLOCKED" if blockers else ("READY_WITH_WARNINGS" if warnings else "READY")
    return {
        "admission_status": admission_status,
        "admission_blockers": blockers,
        "admission_warnings": warnings,
        "normalized_rows": seconds.height,
        "minute_rows": minutes.height,
        "complete_minutes": minutes.filter(pl.col("minute_complete")).height,
        "partial_minutes": minutes.filter(~pl.col("minute_complete")).height,
        "duplicate_key_rows": duplicate_rows,
        "first_timestamp": first_timestamp.isoformat() if first_timestamp else None,
        "last_timestamp": last_timestamp.isoformat() if last_timestamp else None,
        "sessions": seconds["session_date"].n_unique(),
        "contracts": seconds["contract_id"].n_unique(),
        "expiries": seconds["expiry"].n_unique(),
        "minimum_expiries_per_session": minimum_expiries,
        "maximum_expiries_per_session": expiry_counts["expiry"].max(),
        "required_minimum_expiries_per_session": required_expiries,
        "formation_eligible_rows": formation_eligible,
        "outside_session_rows": outside_session,
        "outside_coverage_rows": outside_coverage,
        "out_of_order_within_contract_rows": out_of_order,
        "within_contract_cadence_gaps": gaps,
        "missing_iv_rows": seconds["calendar_iv"].null_count(),
        "invalid_iv_rows": invalid_iv,
        "invalid_forward_rows": invalid_forward,
        "invalid_ttm_rows": invalid_ttm,
        "missing_spot_rows": seconds["spot"].null_count(),
        "missing_bid_rows": seconds["bid"].null_count(),
        "missing_ask_rows": seconds["ask"].null_count(),
        "missing_volume_rows": seconds["volume"].null_count(),
    }
