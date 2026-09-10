"""Run the contract, normalization, and causal minute-aggregation audit."""

from __future__ import annotations

import argparse
import json
import uuid
from pathlib import Path

import polars as pl
from ivshock_validation.audit import audit_frames
from ivshock_validation.contracts import load_source_contract, validate_source_contract
from ivshock_validation.io import scan_source
from ivshock_validation.manifest import (
    git_dirty,
    git_sha,
    sha256_file,
    sha256_json,
    write_manifest,
)
from ivshock_validation.normalize import normalize_source
from ivshock_validation.resample import resample_one_minute


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--input", type=Path, required=True)
    parser.add_argument("--source-contract", type=Path, required=True)
    parser.add_argument("--cache-dir", type=Path, required=True)
    parser.add_argument("--manifest-out", type=Path, required=True)
    parser.add_argument("--stage", choices=["research", "execution"], default="research")
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    if args.manifest_out.exists():
        raise FileExistsError(f"refusing to overwrite manifest: {args.manifest_out}")
    contract = validate_source_contract(
        load_source_contract(args.source_contract), stage=args.stage
    )
    source = scan_source(args.input)
    input_rows = source.select(pl.len()).collect().item()
    normalized = normalize_source(source, contract).collect()
    minute_audit = resample_one_minute(
        normalized.lazy(),
        expected_cadence_seconds=contract["timestamp"]["expected_cadence_seconds"],
    ).collect()
    minutes = minute_audit.filter(pl.col("minute_complete"))
    audit = audit_frames(normalized, minute_audit, contract)

    run_id = f"schema-audit-{uuid.uuid4()}"
    run_dir = args.cache_dir / run_id
    run_dir.mkdir(parents=True, exist_ok=False)
    seconds_path = run_dir / "normalized-seconds.parquet"
    minute_audit_path = run_dir / "minute-audit.parquet"
    minutes_path = run_dir / "sealed-minutes.parquet"
    normalized.write_parquet(seconds_path, compression="zstd", compression_level=9)
    minute_audit.write_parquet(minute_audit_path, compression="zstd", compression_level=9)
    minutes.write_parquet(minutes_path, compression="zstd", compression_level=9)

    contract_sha = sha256_file(args.source_contract)
    input_sha = sha256_file(args.input)
    write_manifest(
        args.manifest_out,
        {
            "run_id": run_id,
            "code_sha": git_sha(),
            "code_dirty": git_dirty(),
            "source_contract_sha256": contract_sha,
            "query_identity": contract["query"]["identity"],
            "query_parameters_sha256": sha256_json(contract["query"]["parameters"]),
            "input_name": args.input.name,
            "input_sha256": input_sha,
            "input_rows": input_rows,
            "source_contract_status": f"ready_for_{args.stage}",
            "resolution_lane": "causally_sealed_1m_replication_preparation",
            "cache_outputs": {
                "run_directory": run_dir.name,
                "normalized_seconds_name": seconds_path.name,
                "normalized_seconds_sha256": sha256_file(seconds_path),
                "minute_audit_name": minute_audit_path.name,
                "minute_audit_sha256": sha256_file(minute_audit_path),
                "sealed_minutes_name": minutes_path.name,
                "sealed_minutes_sha256": sha256_file(minutes_path),
            },
            **audit,
        },
    )
    print(json.dumps({"run_id": run_id, **audit}, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
