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
