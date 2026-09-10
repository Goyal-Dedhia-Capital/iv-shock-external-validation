import json
import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).parents[1]
RUNNER = ROOT / "python" / "run_local.py"
FIXTURE = ROOT / "tests" / "fixtures" / "one_second.csv"
READY_CONTRACT = ROOT / "tests" / "fixtures" / "source_contract.ready.json"
EXAMPLE_CONTRACT = ROOT / "contracts" / "source_contract.example.json"


def invoke(tmp_path: Path, contract: Path) -> subprocess.CompletedProcess[str]:
    environment = {**os.environ, "PYTHONPATH": str(ROOT / "python")}
    return subprocess.run(
        [
            sys.executable,
            str(RUNNER),
            "--input",
            str(FIXTURE),
            "--source-contract",
            str(contract),
            "--cache-dir",
            str(tmp_path / "cache"),
            "--manifest-out",
            str(tmp_path / "manifest.json"),
        ],
        cwd=ROOT,
        env=environment,
        capture_output=True,
        text=True,
        check=False,
    )


def test_preflight_contract_failure_is_compact_json(tmp_path: Path) -> None:
    result = invoke(tmp_path, EXAMPLE_CONTRACT)
    assert result.returncode == 2
    error = json.loads(result.stderr)
    assert error["admission_status"] == "FAILED"
    assert error["error_type"] == "ContractError"
    assert "Traceback" not in result.stderr
    assert not (tmp_path / "manifest.json").exists()


def test_preflight_blocked_admission_is_manifested_and_immutable(tmp_path: Path) -> None:
    first = invoke(tmp_path, READY_CONTRACT)
    assert first.returncode == 2
    manifest = json.loads((tmp_path / "manifest.json").read_text())
    assert manifest["admission_status"] == "BLOCKED"
    assert manifest["authorizes_strategy_execution"] is False
    assert "no_complete_minutes" in manifest["admission_blockers"]
    assert len(list((tmp_path / "cache").glob("*/*"))) == 3

    second = invoke(tmp_path, READY_CONTRACT)
    assert second.returncode == 2
    error = json.loads(second.stderr)
    assert error["error_type"] == "FileExistsError"
    assert len(list((tmp_path / "cache").glob("*"))) == 1
