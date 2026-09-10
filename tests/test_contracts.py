import json
from pathlib import Path

import pytest
from ivshock_validation.contracts import ContractError, validate_source_contract

ROOT = Path(__file__).parents[1]


def test_example_contract_fails_closed() -> None:
    contract = json.loads((ROOT / "contracts/source_contract.example.json").read_text())
    with pytest.raises(ContractError, match="unresolved source-contract fields"):
        validate_source_contract(contract)


def test_fixture_contract_authorizes_execution() -> None:
    contract = json.loads((ROOT / "tests/fixtures/source_contract.ready.json").read_text())
    assert (
        validate_source_contract(contract, stage="execution")["dataset_id"]
        == "synthetic_fixture_v1"
    )


@pytest.mark.parametrize(
    ("mutation", "message"),
    [
        (lambda value: value["query"].update(parameters="all"), "query.parameters"),
        (
            lambda value: value["timestamp"].update(availability_equals_timestamp="yes"),
            "availability_equals_timestamp",
        ),
        (lambda value: value["timestamp"].pop("session_open"), "session_open"),
        (lambda value: value["timestamp"].update(timezone="Mars/Olympus"), "IANA timezone"),
        (
            lambda value: value["coverage"].update(
                first_session="2026-09-09", last_session="2026-09-08"
            ),
            "must not follow",
        ),
        (
            lambda value: value["contracts"].update(provider_contract_id_column=["bad"]),
            "optional source-column mappings",
        ),
        (
            lambda value: value["timestamp"].update(session_open="09:15:00+05:30"),
            "timezone-naive local times",
        ),
        (
            lambda value: value["coverage"].update(minimum_expiries_per_session=2),
            "integer >= 3",
        ),
    ],
)
def test_malformed_contracts_fail_with_actionable_errors(mutation, message: str) -> None:
    contract = json.loads((ROOT / "tests/fixtures/source_contract.ready.json").read_text())
    mutation(contract)
    with pytest.raises(ContractError, match=message):
        validate_source_contract(contract, stage="execution")
