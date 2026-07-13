from __future__ import annotations

import json
import os
import signal
import subprocess
from collections.abc import Iterator
from pathlib import Path

import pytest


@pytest.fixture(scope="session")
def repository_root() -> Path:
    return Path(__file__).resolve().parents[3]


@pytest.fixture(scope="session")
def fixture_request(repository_root: Path) -> dict[str, object]:
    value = json.loads((repository_root / "fixtures/sdk/generate-request.json").read_text())
    assert isinstance(value, dict)
    return value


@pytest.fixture(scope="session")
def bridge_url(repository_root: Path) -> Iterator[str]:
    binary = Path(
        os.environ.get(
            "IMAGEGEN_BRIDGE_SDK_MOCK",
            repository_root / "target/debug/imagegen-bridge-sdk-mock-server",
        )
    )
    process = subprocess.Popen(
        [str(binary)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert process.stdout is not None
    line = process.stdout.readline()
    metadata = json.loads(line)
    try:
        yield str(metadata["base_url"])
    finally:
        process.send_signal(signal.SIGINT)
        _, diagnostics = process.communicate(timeout=5)
        assert process.returncode == 0, diagnostics
