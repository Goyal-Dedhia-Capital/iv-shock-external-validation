"""Fail-closed validation for the data-owner source contract."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any


class ContractError(ValueError):
    """Raised when a source contract cannot authorize the requested stage."""


RESEARCH_REQUIRED = (
    "dataset_id",
    "provider",
    "query.identity",
    "query.parameters",
    "coverage.first_session",
    "coverage.last_session",
    "timestamp.timezone",
    "timestamp.meaning",
    "timestamp.availability_equals_timestamp",
    "timestamp.session_close",
    "volume.meaning",
    "volume.negative_one_meaning",
    "open_interest.meaning",
    "iv.price_basis",
    "iv.model",
    "iv.rate_policy",
    "iv.dividend_policy",
    "iv.ttm_units",
    "iv.expiry_timestamp",
    "iv.solver_failure_representation",
    "underlying.spot_column",
    "underlying.forward_method",
    "contracts.lot_size_authority",
    "resampling.second_rows_are",
    "resampling.volume_aggregation",
)

EXECUTION_REQUIRED = (
    "quotes.buy_price_meaning",
    "quotes.sell_price_meaning",
    "quotes.bid_column",
    "quotes.ask_column",
    "quotes.top_of_book",
    "quotes.observed_not_modeled",
    "quotes.unchanged_quote_policy",
)


def load_source_contract(path: str | Path) -> dict[str, Any]:
    with Path(path).open(encoding="utf-8") as handle:
        contract = json.load(handle)
    if not isinstance(contract, dict):
        raise ContractError("source contract must be a JSON object")
    return contract


def _lookup(document: dict[str, Any], dotted: str) -> Any:
    value: Any = document
    for part in dotted.split("."):
        if not isinstance(value, dict) or part not in value:
            return None
        value = value[part]
    return value


def validate_source_contract(
    contract: dict[str, Any], *, stage: str = "research"
) -> dict[str, Any]:
    if contract.get("contract_version") != 1:
        raise ContractError("contract_version must equal 1")
    if stage not in {"research", "execution"}:
        raise ContractError(f"unsupported stage: {stage}")

    required = RESEARCH_REQUIRED + (EXECUTION_REQUIRED if stage == "execution" else ())
    unresolved = [
        key
        for key in required
        if (value := _lookup(contract, key)) is None or value == "PENDING" or value == ""
    ]

    required_true = (
        "coverage.missing_sessions_documented",
        "coverage.all_required_expiries_requested",
    )
    unresolved.extend(key for key in required_true if _lookup(contract, key) is not True)

    availability = _lookup(contract, "timestamp.availability_equals_timestamp")
    if availability is False and not _lookup(contract, "timestamp.available_at_column"):
        unresolved.append("timestamp.available_at_column")

    if stage == "execution":
        for key in ("quotes.top_of_book", "quotes.observed_not_modeled"):
            if _lookup(contract, key) is not True:
                unresolved.append(f"{key}=true")

    unresolved = sorted(set(unresolved))
    if unresolved:
        raise ContractError("unresolved source-contract fields: " + ", ".join(unresolved))

    if _lookup(contract, "resampling.volume_aggregation") != "sum_incremental":
        raise ContractError("v1 supports only explicitly declared sum_incremental volume")
    if _lookup(contract, "volume.negative_one_meaning") != "missing":
        raise ContractError("v1 requires the owner to declare volume -1 as missing")
    if _lookup(contract, "resampling.second_rows_are") != "one_second_ohlc_bars":
        raise ContractError("v1 supports only explicitly declared one_second_ohlc_bars")

    return contract
