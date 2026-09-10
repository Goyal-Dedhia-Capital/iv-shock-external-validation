"""Causal one-second to sealed one-minute aggregation."""

from __future__ import annotations

import polars as pl

GROUP_KEYS = ["session_date", "ticker", "contract_id", "expiry", "strike", "option_type"]


def resample_one_minute(frame: pl.LazyFrame, *, expected_cadence_seconds: int = 1) -> pl.LazyFrame:
    if expected_cadence_seconds <= 0 or 60 % expected_cadence_seconds:
        raise ValueError("expected cadence must be a positive divisor of 60 seconds")
    expected_observations = 60 // expected_cadence_seconds
    ordered = frame.sort(["contract_id", "timestamp"]).with_columns(
        pl.col("timestamp").dt.truncate("1m").alias("minute")
    )
    last_fields = [
        "spot",
        "forward",
        "bid",
        "ask",
        "open_interest",
        "calendar_ttm",
        "calendar_iv",
        "delta",
        "gamma",
        "theta",
        "vega",
        "model_status",
        "data_source",
        "lot_size",
    ]
    return (
        ordered.group_by([*GROUP_KEYS, "minute"], maintain_order=True)
        .agg(
            pl.col("open").drop_nulls().first().alias("open"),
            pl.col("high").max().alias("high"),
            pl.col("low").min().alias("low"),
            pl.col("close").drop_nulls().last().alias("close"),
            pl.when(pl.col("volume").is_not_null().any())
            .then(pl.col("volume").sum())
            .otherwise(None)
            .alias("volume"),
            pl.col("available_at").max().alias("available_at"),
            *[pl.col(name).drop_nulls().last().alias(name) for name in last_fields],
            pl.len().alias("source_rows"),
            pl.col("timestamp").n_unique().alias("observed_seconds"),
            (
                (pl.col("timestamp").dt.nanosecond() == 0)
                & ((pl.col("timestamp").dt.second() % expected_cadence_seconds) == 0)
            )
            .all()
            .alias("cadence_slots_aligned"),
        )
        .with_columns(
            (pl.col("observed_seconds") == expected_observations).alias("cadence_complete"),
            (pl.col("available_at") <= pl.col("minute") + pl.duration(minutes=1)).alias(
                "availability_complete"
            ),
        )
        .with_columns(
            (
                pl.col("cadence_complete")
                & pl.col("cadence_slots_aligned")
                & pl.col("availability_complete")
            ).alias("minute_complete")
        )
        .sort(["minute", "contract_id"])
    )
