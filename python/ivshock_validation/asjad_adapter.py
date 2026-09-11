"""Asjad export adaptation without importing the owner's private API.

The data owner calls ``marketdata`` outside this repository and writes the
three input tables.  This module performs only exact-time/key joins.  It never
as-of fills spot, invents availability, substitutes forward for spot, or treats
traded volume as executable depth.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

import polars as pl

OPTION_KEY = ["timestamp", "ticker", "expiry_date", "strike_price", "option_type"]
SPOT_KEY = ["timestamp", "ticker"]
LOT_KEY = ["session_date", "ticker"]

ASJAD_REQUIRED = {
    *OPTION_KEY,
    "open",
    "high",
    "low",
    "close",
    "volume",
    "oi",
    "buy_price",
    "sell_price",
    "forward",
    "ttm",
    "iv",
    "delta",
    "gamma",
    "theta",
    "vega",
}


@dataclass(frozen=True)
class AdapterConfig:
    timezone: str = "Asia/Kolkata"
    options_availability_equals_timestamp: bool = False
    spot_availability_equals_timestamp: bool = False


def _schema_names(frame: pl.LazyFrame) -> set[str]:
    return set(frame.collect_schema().names())


def _timestamp(expression: pl.Expr, dtype: pl.DataType, timezone: str) -> pl.Expr:
    if dtype == pl.String:
        return expression.str.to_datetime(strict=True, time_zone=timezone)
    if isinstance(dtype, pl.Datetime):
        if dtype.time_zone is None:
            return expression.dt.replace_time_zone(timezone)
        return expression.dt.convert_time_zone(timezone)
    raise ValueError(f"timestamp column must be string or datetime, got {dtype}")


def _available_at(
    frame: pl.LazyFrame,
    *,
    timestamp_name: str,
    availability_name: str,
    equals_timestamp: bool,
    timezone: str,
) -> pl.Expr:
    schema = frame.collect_schema()
    if availability_name in schema.names():
        return _timestamp(pl.col(availability_name), schema[availability_name], timezone)
    if equals_timestamp:
        return _timestamp(pl.col(timestamp_name), schema[timestamp_name], timezone)
    raise ValueError(
        f"{availability_name} is missing; explicit availability-equals-timestamp "
        "attestation is required"
    )


def adapt_asjad_frames(
    options: pl.LazyFrame,
    *,
    spot: pl.LazyFrame | None = None,
    lots: pl.LazyFrame | None = None,
    config: AdapterConfig | None = None,
) -> pl.DataFrame:
    """Return an enriched Asjad source table accepted by contract normalization.

    ``spot`` must be exact-time data keyed by ``timestamp,ticker``. ``lots``
    must already be expanded to one authoritative row per ``session_date,ticker``.
    This deliberately avoids an implicit backward/forward or effective-date join.
    """

    config = config or AdapterConfig()
    option_schema = options.collect_schema()
    missing = sorted(ASJAD_REQUIRED - set(option_schema.names()))
    if missing:
        raise ValueError("Asjad options export is missing columns: " + ", ".join(missing))

    timestamp = _timestamp(pl.col("timestamp"), option_schema["timestamp"], config.timezone)
    option_available = _available_at(
        options,
        timestamp_name="timestamp",
        availability_name="available_at",
        equals_timestamp=config.options_availability_equals_timestamp,
        timezone=config.timezone,
    )
    enriched = options.with_columns(
        timestamp.alias("timestamp"),
        option_available.alias("_option_available_at"),
        pl.col("option_type").cast(pl.String).str.to_uppercase(),
    ).with_columns(pl.col("timestamp").dt.date().alias("session_date"))

    if "spot" not in option_schema.names():
        if spot is None:
            raise ValueError("authoritative spot input is required; forward cannot substitute")
        spot_schema = spot.collect_schema()
        missing_spot = sorted(set(SPOT_KEY + ["spot"]) - set(spot_schema.names()))
        if missing_spot:
            raise ValueError("spot export is missing columns: " + ", ".join(missing_spot))
        spot_timestamp = _timestamp(
            pl.col("timestamp"), spot_schema["timestamp"], config.timezone
        )
        spot_available = _available_at(
            spot,
            timestamp_name="timestamp",
            availability_name="available_at",
            equals_timestamp=config.spot_availability_equals_timestamp,
            timezone=config.timezone,
        )
        prepared_spot = spot.with_columns(
            spot_timestamp.alias("timestamp"),
            spot_available.alias("_spot_available_at"),
            pl.col("spot").cast(pl.Float64, strict=True),
        ).select(*SPOT_KEY, "spot", "_spot_available_at")
        if prepared_spot.select(pl.struct(SPOT_KEY).is_duplicated().any()).collect().item():
            raise ValueError("spot export has duplicate timestamp,ticker keys")
        enriched = enriched.join(prepared_spot, on=SPOT_KEY, how="left", validate="m:1")
    else:
        enriched = enriched.with_columns(
            pl.col("spot").cast(pl.Float64, strict=True),
            pl.col("_option_available_at").alias("_spot_available_at"),
        )

    if "lot_size" not in option_schema.names():
        if lots is None:
            raise ValueError("authoritative session lot-size input is required")
        lot_names = _schema_names(lots)
        missing_lots = sorted(set(LOT_KEY + ["lot_size"]) - lot_names)
        if missing_lots:
            raise ValueError("lot-size export is missing columns: " + ", ".join(missing_lots))
        lot_schema = lots.collect_schema()
        lot_date = pl.col("session_date")
        if lot_schema["session_date"] == pl.String:
            lot_date = lot_date.str.to_date(strict=True)
        prepared_lots = lots.with_columns(
            lot_date.alias("session_date"),
            pl.col("lot_size").cast(pl.Int64, strict=True),
        ).select(*LOT_KEY, "lot_size")
        if prepared_lots.select(pl.struct(LOT_KEY).is_duplicated().any()).collect().item():
            raise ValueError("lot-size export has duplicate session_date,ticker keys")
        enriched = enriched.join(prepared_lots, on=LOT_KEY, how="left", validate="m:1")
    else:
        enriched = enriched.with_columns(pl.col("lot_size").cast(pl.Int64, strict=True))

    result = (
        enriched.with_columns(
            pl.max_horizontal("_option_available_at", "_spot_available_at").alias("available_at"),
            (
                (pl.col("_option_available_at") < pl.col("timestamp"))
                | (pl.col("_spot_available_at") < pl.col("timestamp"))
            ).alias("_availability_invalid"),
        )
        .drop("_option_available_at", "_spot_available_at")
        .collect()
    )
    if result.select(pl.struct(OPTION_KEY).is_duplicated().any()).item():
        raise ValueError("Asjad options export has duplicate economic row keys")
    missing_spot = result.filter(pl.col("spot").is_null() | (pl.col("spot") <= 0)).height
    if missing_spot:
        raise ValueError(
            f"exact-time authoritative spot is missing or invalid for {missing_spot} rows"
        )
    invalid_lots = result.filter(pl.col("lot_size").is_null() | (pl.col("lot_size") <= 0)).height
    if invalid_lots:
        raise ValueError(f"authoritative lot size is missing or invalid for {invalid_lots} rows")
    if result.filter(pl.col("_availability_invalid")).height:
        raise ValueError("availability precedes an observation")
    return result.drop("_availability_invalid")


def write_parquet_atomic(frame: pl.DataFrame, destination: str | Path) -> None:
    path = Path(destination)
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp")
    frame.write_parquet(temporary, compression="zstd", compression_level=9)
    temporary.replace(path)
