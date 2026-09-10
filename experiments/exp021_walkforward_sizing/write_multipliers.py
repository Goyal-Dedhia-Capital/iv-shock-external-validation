#!/usr/bin/env python3
"""Write the sparse, SHA-addressable EXP021 2x multiplier tape."""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path

import polars as pl


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--predictions", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    rows = (
        pl.read_parquet(args.predictions)
        .filter(
            (pl.col("feature_set") == "spot_iv_crowding")
            & (pl.col("multiplier") == 2)
        )
        .select("strategy_position_id")
        .to_series()
        .to_list()
    )
    tape = {value: 2 for value in rows}
    if len(tape) != len(rows):
        raise ValueError("duplicate candidate identifiers in multiplier tape")
    body = (json.dumps(tape, indent=2, sort_keys=True) + "\n").encode()
    args.output.write_bytes(body)
    print(hashlib.sha256(body).hexdigest())


if __name__ == "__main__":
    main()
