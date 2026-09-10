#!/usr/bin/env python3
"""Launch one persistent Rust runner only after local contracts are bound."""
from __future__ import annotations

import argparse
import hashlib
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
    parser.add_argument(
        "--runner", choices=("detector", "books", "policy", "exp021", "exp046"), required=True
    )
    parser.add_argument("--calendar", type=Path, required=True)
    parser.add_argument("--source-contract", type=Path, required=True)
    parser.add_argument("--stage", choices=("research", "execution"), default="research")
    parser.add_argument("--policy-config-json")
    parser.add_argument("--model-bundle", type=Path)
    parser.add_argument("--multipliers", type=Path)
    args = parser.parse_args()

    _, calendar_hash = load_bound_json(args.calendar, kind="exchange calendar")
    _, source_hash = load_validated_source_contract(args.source_contract, stage=args.stage)
    model_hash = ""
    if args.model_bundle is not None:
        _, model_hash = load_bound_json(args.model_bundle, kind="model bundle")
    config_hash = hashlib.sha256(
        (args.policy_config_json or "").encode("utf-8")
    ).hexdigest()
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
            ".gitignore",
            "Cargo.lock",
            "contracts",
            "docs",
            "experiments",
            "models",
            "python",
            "pyproject.toml",
            "scripts",
            "rust",
            "rust-toolchain.toml",
            "uv.lock",
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
        "exp021": ("exp021-walkforward-sizing", "exp021-policy"),
        "exp046": ("exp046-r2-core-policy", "exp046-policy"),
    }
    package, binary = packages[args.runner]
    command = ["cargo", "run", "--locked", "--release"]
    if args.runner in {"exp021", "exp046"}:
        experiment = "exp021_walkforward_sizing" if args.runner == "exp021" else "exp046_r2_core"
        manifest = ROOT / "experiments" / experiment / "Cargo.toml"
        command += ["--manifest-path", str(manifest), "--bin", binary]
        config_text = (manifest.parent / "policy_config.json").read_text()
        command += ["--", "--config-json", config_text]
        config_hash = hashlib.sha256(config_text.encode()).hexdigest()
    else:
        command += [
            "--manifest-path", str(ROOT / "rust/Cargo.toml"), "-p", package, "--bin", binary
        ]
    if args.runner == "policy" and args.policy_config_json:
        command += ["--", "--config-json", args.policy_config_json]
    additional_hash = ""
    embedded_hash = ""
    tape_hash = ""
    if args.runner == "exp046":
        embedded_model = ROOT / "experiments/exp046_r2_core/MODEL.json"
        embedded_hash = hashlib.sha256(embedded_model.read_bytes()).hexdigest()
        if embedded_hash != "82e757f6787238222f4595fe106f7238281104e8292f25af79cfcf5c5208a16a":
            raise ValueError("EXP046 embedded model SHA-256 mismatch")
        additional_hash = embedded_hash
    if args.runner == "exp021":
        if args.multipliers is None:
            raise ValueError("--multipliers is required for EXP021")
        tape_hash = hashlib.sha256(args.multipliers.read_bytes()).hexdigest()
        additional_hash = tape_hash
    environment = os.environ.copy()
    environment.update(
        IV_SHOCK_CALENDAR_SHA256=calendar_hash,
        IV_SHOCK_CALENDAR_PATH=str(args.calendar.resolve()),
        IV_SHOCK_SOURCE_CONTRACT_SHA256=source_hash,
        IV_SHOCK_RESEARCH_BUNDLE_HASH=run_bundle_hash(
            revision, calendar_hash, source_hash, model_hash, config_hash, additional_hash
        ),
        IV_SHOCK_POLICY_CONFIG_SHA256=config_hash,
    )
    if args.model_bundle is not None:
        environment.update(
            IV_SHOCK_MODEL_BUNDLE_PATH=str(args.model_bundle.resolve()),
            IV_SHOCK_MODEL_BUNDLE_SHA256=model_hash,
        )
    if args.runner == "exp046":
        environment["IV_SHOCK_MODEL_BUNDLE_SHA256"] = embedded_hash
    if args.runner == "exp021":
        environment.update(
            EXP021_MULTIPLIERS_JSON=str(args.multipliers.resolve()),
            EXP021_MULTIPLIERS_SHA256=tape_hash,
        )
    os.execvpe(command[0], command, environment)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (ValueError, subprocess.CalledProcessError) as error:
        print(f"BLOCKED: {error}", file=sys.stderr)
        raise SystemExit(2) from None
