"""Parsel: Algorithmic Reasoning with Language Models by Composing
Decompositions (Zelikman et al. 2023, arXiv:2212.10561) — a function graph,
implemented part by part, then composed.

Parsel separates *what* from *how*: first decompose the problem into a
graph of function specifications (name, signature, one-line contract,
dependencies), then implement each function independently, then compose
and test the whole. The h5i mapping is one validated-JSON ``ask`` for the
decomposition, a ``map_reduce`` fan-out — one work assignment per spec,
the reducer composing the module per the dependency graph — and neutral
verification of the composition. The compositional counterpart of
``mapcoder.py``'s plan switching.

    python examples/papers/parsel.py ["<task>"]   # default: implement quicksort with pytest
"""

import asyncio
import json
import sys
from collections.abc import Mapping
from typing import Any

from h5i.orchestra import Conductor, patterns

DEMO_TASK = "implement quicksort with pytest"
VERIFY = ["pytest", "-q"]
MAX_FUNCTIONS = 4


def parse_functions(value: Any) -> list[dict[str, Any]]:
    items = value.get("functions") if isinstance(value, Mapping) else None
    if not isinstance(items, list) or not items:
        raise ValueError(
            'reply must be {"functions": [{"name": "...", "signature": "...", '
            '"contract": "...", "uses": ["<other function names>"]}]}'
        )
    functions = []
    for f in items[:MAX_FUNCTIONS]:
        if not isinstance(f, Mapping) or "name" not in f:
            raise ValueError("every function needs at least a name")
        functions.append(
            {
                "name": str(f["name"]).strip(),
                "signature": str(f.get("signature", "")).strip(),
                "contract": str(f.get("contract", "")).strip(),
                "uses": [str(u) for u in (f.get("uses") or [])],
            }
        )
    return functions


async def main(task: str) -> None:
    async with Conductor(".", "parsel-demo", launcher="resident", isolation="supervised") as c:
        decomposer = await c.hire("decomposer", runtime="claude", model="claude-haiku-4-5")
        coders = [
            await c.hire("coder0", runtime="claude", model="claude-haiku-4-5"),
            await c.hire("coder1", runtime="codex", model="gpt-5.4-mini", effort="medium"),
        ]
        composer = await c.hire("composer", runtime="claude", model="claude-haiku-4-5")

        # 1. Decompose into a function graph — the "what".
        functions = await decomposer.ask(
            f"Task: {task}\n\nDecompose it into at most {MAX_FUNCTIONS} "
            "functions. For each give: name, signature, a one-line contract, "
            "and which of the other functions it uses. Reply as JSON: "
            '{"functions": [{"name": "...", "signature": "...", '
            '"contract": "...", "uses": ["..."]}]}',
            parse=parse_functions,
        )
        graph = json.dumps(functions, indent=2)
        print("function graph:")
        for f in functions:
            uses = f" ← uses {', '.join(f['uses'])}" if f["uses"] else ""
            print(f"  {f['name']}{f['signature']}{uses}")

        # 2-3. Implement each spec independently, then compose — the "how".
        # map_reduce: cross-agent parallel, and the reducer gets every part
        # as sealed-phase materials.
        outcome = await patterns.map_reduce(
            c,
            [
                (
                    coders[i % len(coders)],
                    (
                        f"Implement EXACTLY this function, with unit tests for "
                        f"it (assume the other functions in the graph exist as "
                        f"specified):\n{json.dumps(f, indent=2)}\n\n"
                        f"Full graph for context:\n{graph}\n\nPart of: {task}"
                    ),
                )
                for i, f in enumerate(functions)
            ],
            reduce=(
                composer,
                (
                    f"Compose the granted per-function implementations into one "
                    f"coherent module for: {task}\nWire them per this dependency "
                    f"graph, resolve duplicate helpers, keep all tests:\n{graph}"
                ),
            ),
        )
        merged = outcome.merged
        assert merged is not None

        # 4. Test the composition, not the parts.
        await c.verify(merged, VERIFY)
        verdict = await c.judge()
        print("verdict:", verdict.selected_submission or "none", "—", *verdict.reasons)


if __name__ == "__main__":
    asyncio.run(main(sys.argv[1] if len(sys.argv) > 1 else DEMO_TASK))
