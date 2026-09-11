#!/usr/bin/env python3
"""Join private Asjad exports into the source-contract input boundary."""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

from ivshock_validation.asjad_adapter import (
    AdapterConfig,
    adapt_asjad_frames,
    write_parquet_atomic,
)
from ivshock_validation.io import scan_source
from ivshock_validation.manifest import sha256_file, write_manifest


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--options", type=Path, required=True)
    parser.add_argument("--spot", type=Path)
    parser.add_argument("--lots", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--manifest-out", type=Path, required=True)
    parser.add_argument("--timezone", default="Asia/Kolkata")
    parser.add_argument("--options-availability-equals-timestamp", action="store_true")
    parser.add_argument("--spot-availability-equals-timestamp", action="store_true")
    args = parser.parse_args()
    if args.output.exists() or args.manifest_out.exists():
        raise FileExistsError("refusing to overwrite adapter output or manifest")
    frame = adapt_asjad_frames(
        scan_source(args.options),
        spot=scan_source(args.spot) if args.spot else None,
        lots=scan_source(args.lots) if args.lots else None,
        config=AdapterConfig(
            timezone=args.timezone,
            options_availability_equals_timestamp=args.options_availability_equals_timestamp,
            spot_availability_equals_timestamp=args.spot_availability_equals_timestamp,
        ),
    )
    write_parquet_atomic(frame, args.output)
    manifest = {
        "schema_version": "gdc.asjad-adapter.v1",
        "options_sha256": sha256_file(args.options),
        "spot_sha256": sha256_file(args.spot) if args.spot else None,
        "lots_sha256": sha256_file(args.lots) if args.lots else None,
        "output_sha256": sha256_file(args.output),
        "rows": frame.height,
        "sessions": frame["session_date"].n_unique(),
        "expiries": frame["expiry_date"].n_unique(),
        "first_timestamp": frame["timestamp"].min().isoformat(),
        "last_timestamp": frame["timestamp"].max().isoformat(),
        "availability_policy": {
            "options_equals_timestamp": args.options_availability_equals_timestamp,
            "spot_equals_timestamp": args.spot_availability_equals_timestamp,
        },
        "warning": "volume is activity only and is not executable quote depth",
    }
    write_manifest(args.manifest_out, manifest)
    print(json.dumps(manifest, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (FileExistsError, ValueError) as error:
        print(json.dumps({"status": "FAILED", "error": str(error)}), file=sys.stderr)
        raise SystemExit(2) from None
