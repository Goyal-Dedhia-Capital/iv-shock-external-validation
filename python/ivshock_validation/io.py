"""Lazy local-source readers."""

from __future__ import annotations

from pathlib import Path

import polars as pl


def scan_source(path: str | Path) -> pl.LazyFrame:
    source = Path(path)
    suffix = source.suffix.lower()
    if suffix == ".csv":
        return pl.scan_csv(source, null_values=["", "null", "NULL", "nan", "NaN"]).filter(
            pl.col("timestamp").is_not_null()
        )
    if suffix in {".parquet", ".pq"}:
        return pl.scan_parquet(source)
    raise ValueError(f"unsupported source format: {source.suffix}")
