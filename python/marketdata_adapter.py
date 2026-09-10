"""Friend-owned private API boundary.

Implement `export_source` on the data-owner machine. Keep credentials and API
objects out of committed files. The strategy and normalization packages must
never import the private marketdata client directly.
"""

from __future__ import annotations

from datetime import date
from pathlib import Path


def export_source(
    *,
    symbol: str,
    start_date: date,
    end_date: date,
    output_path: Path,
) -> Path:
    """Export all required expiries at native one-second resolution.

    The implementation must use the exact query parameters recorded in the run
    manifest and write CSV or Parquet to `output_path`. It must not filter
    contracts based on future endpoint availability.
    """
    raise NotImplementedError("the data owner must implement the private marketdata adapter")
