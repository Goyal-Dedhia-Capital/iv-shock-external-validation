"""Compact, non-sensitive preparation audit."""

from __future__ import annotations

from datetime import time
from typing import Any

import polars as pl

SOURCE_KEY = ["timestamp", "ticker", "expiry", "strike", "option_type"]


def audit_frames(
    seconds: pl.DataFrame, minutes: pl.DataFrame, contract: dict[str, Any]
) -> dict[str, object]:
    duplicate_rows = seconds.height - seconds.unique(SOURCE_KEY).height
    if duplicate_rows:
        raise ValueError(f"duplicate normalized source keys: {duplicate_rows}")
    if seconds.filter(~pl.col("option_type").is_in(["CE", "PE"])).height:
        raise ValueError("option_type must be CE or PE")
    if seconds.filter(pl.col("close").is_not_null() & (pl.col("close") <= 0)).height:
        raise ValueError("non-positive close found")
    crossed = seconds.filter(
        pl.col("bid").is_not_null() & pl.col("ask").is_not_null() & (pl.col("bid") > pl.col("ask"))
    ).height
    if crossed:
        raise ValueError(f"crossed canonical quotes: {crossed}")
    unavailable = seconds.filter(pl.col("available_at") < pl.col("timestamp")).height
    if unavailable:
        raise ValueError(f"availability precedes source timestamp: {unavailable}")

    open_time = time.fromisoformat(contract["timestamp"]["session_open"])
    close_time = time.fromisoformat(contract["timestamp"]["session_close"])
    outside_session = seconds.filter(
        (pl.col("timestamp").dt.time() < open_time) | (pl.col("timestamp").dt.time() > close_time)
    ).height
    if outside_session:
        raise ValueError(f"source rows outside declared session: {outside_session}")

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
    return {
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
        "minimum_expiries_per_session": expiry_counts["expiry"].min(),
        "maximum_expiries_per_session": expiry_counts["expiry"].max(),
        "outside_session_rows": outside_session,
        "out_of_order_within_contract_rows": out_of_order,
        "within_contract_cadence_gaps": gaps,
        "missing_iv_rows": seconds["calendar_iv"].null_count(),
        "missing_spot_rows": seconds["spot"].null_count(),
        "missing_bid_rows": seconds["bid"].null_count(),
        "missing_ask_rows": seconds["ask"].null_count(),
        "missing_volume_rows": seconds["volume"].null_count(),
    }
