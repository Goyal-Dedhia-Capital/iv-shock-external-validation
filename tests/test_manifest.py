from pathlib import Path

import pytest
from ivshock_validation.manifest import write_manifest


def test_manifest_is_immutable(tmp_path: Path) -> None:
    destination = tmp_path / "manifest.json"
    write_manifest(destination, {"run_id": "first"})
    with pytest.raises(FileExistsError):
        write_manifest(destination, {"run_id": "second"})
