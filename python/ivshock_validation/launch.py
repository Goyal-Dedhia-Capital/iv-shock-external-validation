"""Fail-closed binding for local strategy launches."""
from __future__ import annotations

import hashlib
import json
from pathlib import Path
from typing import Any

from ivshock_validation.contracts import validate_source_contract


def sha256_file(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _pending_paths(value: Any, prefix: str = "$") -> list[str]:
    if isinstance(value, dict):
        return [
            path
            for key, child in value.items()
            for path in _pending_paths(child, f"{prefix}.{key}")
        ]
    if isinstance(value, list):
        return [
            path
            for index, child in enumerate(value)
            for path in _pending_paths(child, f"{prefix}[{index}]")
        ]
    return [prefix] if isinstance(value, str) and value.strip().upper() == "PENDING" else []


def load_bound_json(path: Path, *, kind: str) -> tuple[dict[str, Any], str]:
    """Load a launch contract and reject placeholders or malformed roots."""
    try:
        value = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"{kind} is unreadable or invalid JSON: {error}") from error
    if not isinstance(value, dict):
        raise ValueError(f"{kind} must be a JSON object")
    pending = _pending_paths(value)
    if pending:
        raise ValueError(f"{kind} contains PENDING fields: {', '.join(pending[:8])}")
    if kind == "exchange calendar":
        if value.get("status") != "READY":
            raise ValueError("exchange calendar status must be READY")
        if value.get("observed_quotes_may_define_session_bounds") is not False:
            raise ValueError("observed quotes may not define session bounds")
    return value, sha256_file(path)


def run_bundle_hash(
    strategy_revision: str,
    calendar_hash: str,
    source_hash: str,
    *additional_hashes: str,
) -> str:
    """Bind strategy revision, calendar, and source contract into one identity."""
    material = "\n".join(
        (strategy_revision, calendar_hash, source_hash, *additional_hashes)
    ).encode()
    return hashlib.sha256(material).hexdigest()


def load_validated_source_contract(
    path: Path, *, stage: str
) -> tuple[dict[str, Any], str]:
    """Load, structurally validate, and hash the exact source contract."""
    value, digest = load_bound_json(path, kind="source contract")
    validate_source_contract(value, stage=stage)
    return value, digest
