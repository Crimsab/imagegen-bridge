from __future__ import annotations

import re
from pathlib import Path

import tomllib

ROOT = Path(__file__).resolve().parents[3]
SDK = ROOT / "sdks" / "python"


def test_python_build_backend_has_exact_hashed_closure() -> None:
    manifest = tomllib.loads((SDK / "pyproject.toml").read_text())
    requirements = manifest["build-system"]["requires"]
    assert requirements == ["hatchling==1.31.0"]

    constraints = (SDK / "build-constraints.txt").read_text()
    records = re.split(r"\n(?=[a-zA-Z0-9_.-]+==)", constraints.strip())
    names = set()
    for record in records:
        first = record.splitlines()[0]
        match = re.fullmatch(r"([a-zA-Z0-9_.-]+)==[^\s]+ \\", first)
        assert match is not None, first
        names.add(match.group(1))
        assert record.count("--hash=sha256:") >= 1
    assert names == {"hatchling", "packaging", "pathspec", "pluggy", "trove-classifiers"}


def test_ci_uses_immutable_actions_and_nonimplicit_python_builds() -> None:
    workflows = sorted((ROOT / ".github" / "workflows").glob("*.y*ml"))
    assert workflows
    action = re.compile(r"uses:\s*([^\s]+)@([^\s#]+)")
    for workflow in workflows:
        for line in workflow.read_text().splitlines():
            match = action.search(line)
            if match is None or match.group(1).startswith("./"):
                continue
            assert re.fullmatch(r"[0-9a-f]{40}", match.group(2)), line

    ci = (ROOT / ".github" / "workflows" / "ci.yml").read_text()
    assert "uv sync --locked --extra test --no-install-project" in ci
    assert "uv build --build-constraints build-constraints.txt --require-hashes" in ci
    for line in ci.splitlines():
        if "run: uv run " in line:
            assert "uv run --no-sync " in line
