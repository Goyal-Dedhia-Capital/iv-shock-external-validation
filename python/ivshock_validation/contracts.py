"""Fail-closed validation for the data-owner source contract."""

from __future__ import annotations

import json
from datetime import date, time
from pathlib import Path
from typing import Any
from zoneinfo import ZoneInfo, ZoneInfoNotFoundError


class ContractError(ValueError):
    """Raised when a source contract cannot authorize the requested stage."""


RESEARCH_REQUIRED = (
    "dataset_id",
    "provider",
    "symbol",
    "query.identity",
    "query.parameters",
    "coverage.first_session",
    "coverage.last_session",
    "coverage.minimum_expiries_per_session",
    "coverage.expiry_inventory_evidence",
    "timestamp.timezone",
    "timestamp.meaning",
    "timestamp.availability_equals_timestamp",
    "timestamp.expected_cadence_seconds",
    "timestamp.session_open",
    "timestamp.session_close",
    "volume.meaning",
    "volume.column",
    "volume.negative_one_meaning",
    "open_interest.meaning",
    "open_interest.column",
    "iv.column",
    "iv.price_basis",
    "iv.model",
    "iv.rate_policy",
    "iv.dividend_policy",
    "iv.ttm_units",
    "iv.ttm_column",
    "iv.expiry_timestamp",
    "iv.solver_failure_representation",
    "underlying.spot_column",
    "underlying.forward_column",
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
    "contracts.lot_size_column",
)

STRING_REQUIRED = tuple(
    key
    for key in RESEARCH_REQUIRED + EXECUTION_REQUIRED
    if key
    not in {
        "query.parameters",
        "timestamp.availability_equals_timestamp",
        "timestamp.expected_cadence_seconds",
        "coverage.minimum_expiries_per_session",
        "quotes.top_of_book",
        "quotes.observed_not_modeled",
    }
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
    if isinstance(contract.get("contract_version"), bool) or contract.get("contract_version") != 1:
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

    active_string_fields = [key for key in STRING_REQUIRED if key in required]
    malformed_strings = [
        key for key in active_string_fields if not isinstance(_lookup(contract, key), str)
    ]
    if malformed_strings:
        raise ContractError(
            "source-contract fields must be strings: " + ", ".join(sorted(malformed_strings))
        )

    if not isinstance(_lookup(contract, "query.parameters"), dict):
        raise ContractError("query.parameters must be a JSON object")
    if not isinstance(availability, bool):
        raise ContractError("timestamp.availability_equals_timestamp must be boolean")
    if availability is False and not isinstance(
        _lookup(contract, "timestamp.available_at_column"), str
    ):
        raise ContractError("timestamp.available_at_column must be a string when required")
    optional_columns = (
        "quotes.bid_column",
        "quotes.ask_column",
        "contracts.provider_contract_id_column",
        "contracts.lot_size_column",
    )
    malformed_optional_columns = []
    for key in optional_columns:
        value = _lookup(contract, key)
        if value is not None and value != "PENDING" and not isinstance(value, str):
            malformed_optional_columns.append(key)
    if malformed_optional_columns:
        raise ContractError(
            "optional source-column mappings must be strings or null: "
            + ", ".join(sorted(malformed_optional_columns))
        )
    cadence = _lookup(contract, "timestamp.expected_cadence_seconds")
    if isinstance(cadence, bool) or not isinstance(cadence, int) or cadence <= 0 or 60 % cadence:
        raise ContractError("timestamp.expected_cadence_seconds must be a positive divisor of 60")
    try:
        ZoneInfo(_lookup(contract, "timestamp.timezone"))
    except (TypeError, ZoneInfoNotFoundError) as exc:
        raise ContractError("timestamp.timezone must be a valid IANA timezone") from exc
    try:
        first_session = date.fromisoformat(_lookup(contract, "coverage.first_session"))
        last_session = date.fromisoformat(_lookup(contract, "coverage.last_session"))
    except (TypeError, ValueError) as exc:
        raise ContractError("coverage sessions must be ISO dates") from exc
    if first_session > last_session:
        raise ContractError("coverage.first_session must not follow coverage.last_session")
    minimum_expiries = _lookup(contract, "coverage.minimum_expiries_per_session")
    if (
        isinstance(minimum_expiries, bool)
        or not isinstance(minimum_expiries, int)
        or minimum_expiries < 3
    ):
        raise ContractError("coverage.minimum_expiries_per_session must be an integer >= 3")
    try:
        session_open = time.fromisoformat(_lookup(contract, "timestamp.session_open"))
        session_close = time.fromisoformat(_lookup(contract, "timestamp.session_close"))
    except (TypeError, ValueError) as exc:
        raise ContractError("session_open/session_close must be ISO local times") from exc
    if session_open.tzinfo is not None or session_close.tzinfo is not None:
        raise ContractError("session_open/session_close must be timezone-naive local times")
    if session_open >= session_close:
        raise ContractError("timestamp.session_open must precede timestamp.session_close")

    if _lookup(contract, "resampling.volume_aggregation") != "sum_incremental":
        raise ContractError("v1 supports only explicitly declared sum_incremental volume")
    if _lookup(contract, "volume.negative_one_meaning") != "missing":
        raise ContractError("v1 requires the owner to declare volume -1 as missing")
    if _lookup(contract, "resampling.second_rows_are") != "one_second_ohlc_bars":
        raise ContractError("v1 supports only explicitly declared one_second_ohlc_bars")
    if _lookup(contract, "resampling.missing_second_policy") != "no_cross_minute_fill":
        raise ContractError("v1 requires no_cross_minute_fill")
    if _lookup(contract, "resampling.oi_aggregation") != "last_non_null":
        raise ContractError("v1 supports only last_non_null OI aggregation")
    if _lookup(contract, "resampling.quote_aggregation") != "last_non_null_within_minute":
        raise ContractError("v1 supports only last_non_null_within_minute quotes")
    if _lookup(contract, "iv.ttm_units") not in {"years", "calendar_days"}:
        raise ContractError("v1 supports iv.ttm_units of years or calendar_days")

    return contract
