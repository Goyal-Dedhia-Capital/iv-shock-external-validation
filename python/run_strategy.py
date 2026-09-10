#!/usr/bin/env python3
"""Launch one persistent Rust runner only after local contracts are bound."""
from __future__ import annotations

import argparse
import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "python"))

from ivshock_validation.launch import (  # noqa: E402
    load_bound_json,
    load_validated_source_contract,
    run_bundle_hash,
)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--runner", choices=("detector", "books", "policy"), required=True)
    parser.add_argument("--calendar", type=Path, required=True)
    parser.add_argument("--source-contract", type=Path, required=True)
    parser.add_argument("--stage", choices=("research", "execution"), default="research")
    parser.add_argument("--policy-config-json")
    args = parser.parse_args()

    _, calendar_hash = load_bound_json(args.calendar, kind="exchange calendar")
    _, source_hash = load_validated_source_contract(args.source_contract, stage=args.stage)
    revision = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=ROOT, check=True, text=True, capture_output=True
    ).stdout.strip()
    dirty = subprocess.run(
        [
            "git",
            "status",
            "--porcelain",
            "--",
            ".github",
            "docs",
            "python",
            "scripts",
            "rust",
            "variants",
        ],
        cwd=ROOT,
        check=True,
        text=True,
        capture_output=True,
    ).stdout.strip()
    if dirty:
        raise ValueError(
            "strategy or registry files are dirty; commit a reviewable candidate first"
        )

    packages = {
        "detector": ("iv-shock-decision", "iv-shock-decision"),
        "books": ("iv-shock-sequential-books", "iv-shock-sequential-books"),
        "policy": ("iv-shock-strategy-policy", "iv-shock-strategy-policy"),
    }
    package, binary = packages[args.runner]
    command = [
        "cargo", "run", "--locked", "--release", "--manifest-path", str(ROOT / "rust/Cargo.toml"),
        "-p", package, "--bin", binary,
    ]
    if args.runner == "policy" and args.policy_config_json:
        command += ["--", "--config-json", args.policy_config_json]
    environment = os.environ.copy()
    environment.update(
        IV_SHOCK_CALENDAR_SHA256=calendar_hash,
        IV_SHOCK_CALENDAR_PATH=str(args.calendar.resolve()),
        IV_SHOCK_SOURCE_CONTRACT_SHA256=source_hash,
        IV_SHOCK_RESEARCH_BUNDLE_HASH=run_bundle_hash(revision, calendar_hash, source_hash),
    )
    os.execvpe(command[0], command, environment)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (ValueError, subprocess.CalledProcessError) as error:
        print(f"BLOCKED: {error}", file=sys.stderr)
        raise SystemExit(2) from None
