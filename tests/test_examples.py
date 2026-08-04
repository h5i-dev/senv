import ast
from pathlib import Path

import pytest

EXAMPLES = Path(__file__).parents[1] / "examples"
CHEAP_MODELS = {"claude-haiku-4-5", "gpt-5.4-mini"}


@pytest.mark.parametrize(
    "path",
    sorted(EXAMPLES.rglob("*.py")),
    ids=lambda p: str(p.relative_to(EXAMPLES)),
)
def test_examples_are_valid_python(path: Path):
    ast.parse(path.read_text(), filename=str(path))


@pytest.mark.parametrize("name", ["arena_score.py", "ensemble_score.py"])
def test_resident_examples_do_not_preflight_live_sessions(name: str):
    tree = ast.parse((EXAMPLES / "tutorial" / name).read_text())
    live_checks = [
        call
        for call in ast.walk(tree)
        if isinstance(call, ast.Call)
        and isinstance(call.func, ast.Attribute)
        and call.func.attr == "preflight"
        and any(keyword.arg == "live" for keyword in call.keywords)
    ]

    assert not live_checks, (
        'launcher="resident" starts sessions lazily on the first turn; '
        "preflight(live=...) fails before that turn can start them"
    )


@pytest.mark.parametrize(
    "path",
    sorted(EXAMPLES.rglob("*.py")),
    ids=lambda p: str(p.relative_to(EXAMPLES)),
)
def test_examples_pin_cheap_models(path: Path):
    tree = ast.parse(path.read_text())
    hire_calls = [
        call
        for call in ast.walk(tree)
        if isinstance(call, ast.Call)
        and isinstance(call.func, ast.Attribute)
        and call.func.attr == "hire"
    ]

    assert all(any(keyword.arg == "model" for keyword in call.keywords) for call in hire_calls)

    explicit_models = {
        node.value
        for node in ast.walk(tree)
        if isinstance(node, ast.Constant)
        and isinstance(node.value, str)
        and (node.value.startswith("claude-") or node.value.startswith("gpt-"))
    }
    assert explicit_models <= CHEAP_MODELS
