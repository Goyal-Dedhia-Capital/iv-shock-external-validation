"""Normalize a provider frame without inventing missing market facts."""

from __future__ import annotations

from typing import Any

import polars as pl

BASE_REQUIRED = {
    "timestamp",
    "ticker",
    "expiry_date",
    "strike_price",
    "option_type",
    "open",
    "high",
    "low",
    "close",
    "volume",
    "oi",
    "forward",
    "ttm",
    "iv",
    "delta",
    "gamma",
    "theta",
    "vega",
}


def _pending(value: Any) -> bool:
    return value in {None, "", "PENDING"}


def normalize_source(frame: pl.LazyFrame, contract: dict[str, Any]) -> pl.LazyFrame:
    schema = frame.collect_schema()
    missing = sorted(BASE_REQUIRED - set(schema.names()))
    if missing:
        raise ValueError("missing required source columns: " + ", ".join(missing))

    timestamp_expr = pl.col("timestamp")
    if schema["timestamp"] == pl.String:
        timestamp_expr = timestamp_expr.str.to_datetime(
            strict=True, time_zone=contract["timestamp"]["timezone"]
        )

    available_column = contract["timestamp"].get("available_at_column")
    if contract["timestamp"]["availability_equals_timestamp"] is True:
        available_expr = pl.col("timestamp")
    else:
        if not available_column or available_column not in schema:
            raise ValueError("declared availability column is missing")
        available_expr = pl.col(available_column)
        if schema[available_column] == pl.String:
            available_expr = available_expr.str.to_datetime(
                strict=True, time_zone=contract["timestamp"]["timezone"]
            )

    expiry_expr = pl.col("expiry_date")
    if schema["expiry_date"] == pl.String:
        expiry_expr = expiry_expr.str.to_date(strict=True)

    spot_column = contract["underlying"]["spot_column"]
    if _pending(spot_column) or spot_column not in schema:
        raise ValueError("declared spot column is missing")

    bid_column = contract["quotes"].get("bid_column")
    ask_column = contract["quotes"].get("ask_column")
    bid_expr = pl.lit(None, dtype=pl.Float64)
    ask_expr = pl.lit(None, dtype=pl.Float64)
    if not _pending(bid_column) and not _pending(ask_column):
        if bid_column not in schema or ask_column not in schema:
            raise ValueError("declared bid/ask columns are missing")
        bid_expr = pl.col(bid_column).cast(pl.Float64, strict=False)
        ask_expr = pl.col(ask_column).cast(pl.Float64, strict=False)

    provider_contract_id = contract["contracts"].get("provider_contract_id_column")
    if provider_contract_id:
        if provider_contract_id not in schema:
            raise ValueError("declared provider contract ID column is missing")
        contract_id_expr = pl.col(provider_contract_id).cast(pl.String)
    else:
        contract_id_expr = pl.concat_str(
            [
                pl.col("ticker"),
                expiry_expr.cast(pl.String),
                pl.col("strike_price").cast(pl.Float64).cast(pl.String),
                pl.col("option_type").str.to_uppercase(),
            ],
            separator="|",
        )

    lot_column = contract["contracts"].get("lot_size_column")
    lot_expr = pl.lit(None, dtype=pl.Int64)
    if lot_column:
        if lot_column not in schema:
            raise ValueError("declared lot-size column is missing")
        lot_expr = pl.col(lot_column).cast(pl.Int64, strict=False)

    negative_one_is_missing = contract["volume"]["negative_one_meaning"] == "missing"
    volume_expr = pl.col("volume").cast(pl.Float64, strict=False)
    if negative_one_is_missing:
        volume_expr = pl.when(volume_expr == -1).then(None).otherwise(volume_expr)

    normalized = frame.with_columns(
        timestamp_expr.alias("timestamp"),
        expiry_expr.alias("expiry"),
        pl.col("strike_price").cast(pl.Float64).alias("strike"),
        pl.col("option_type").str.to_uppercase().alias("option_type"),
        pl.col(spot_column).cast(pl.Float64, strict=False).alias("spot"),
        pl.col("oi").cast(pl.Float64, strict=False).alias("open_interest"),
        pl.col("ttm").cast(pl.Float64, strict=False).alias("calendar_ttm"),
        pl.col("iv").cast(pl.Float64, strict=False).alias("calendar_iv"),
        bid_expr.alias("bid"),
        ask_expr.alias("ask"),
        contract_id_expr.alias("contract_id"),
        lot_expr.alias("lot_size"),
        volume_expr.alias("volume"),
    ).with_columns(
        available_expr.alias("available_at"),
        pl.col("timestamp").dt.date().alias("session_date"),
        pl.when(pl.col("calendar_iv").is_finite() & (pl.col("calendar_iv") > 0))
        .then(pl.lit("ok"))
        .otherwise(pl.lit("missing_or_invalid_iv"))
        .alias("model_status"),
        pl.lit(contract["provider"]).alias("data_source"),
    )

    return normalized.select(
        "session_date",
        "timestamp",
        "available_at",
        "ticker",
        "contract_id",
        "expiry",
        "strike",
        "option_type",
        "spot",
        pl.col("forward").cast(pl.Float64, strict=False),
        *[pl.col(name).cast(pl.Float64, strict=False) for name in ("open", "high", "low", "close")],
        "bid",
        "ask",
        "volume",
        "open_interest",
        "calendar_ttm",
        "calendar_iv",
        *[
            pl.col(name).cast(pl.Float64, strict=False)
            for name in ("delta", "gamma", "theta", "vega")
        ],
        "model_status",
        "data_source",
        "lot_size",
    )
