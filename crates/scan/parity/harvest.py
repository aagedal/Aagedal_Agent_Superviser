"""Collects command-like strings from atif-scan's tests into cases_harvested.json.

    uv run ... python crates/scan/parity/harvest.py PATH/TO/atif-scan/tests

Imports each test module, reads the values of its `pytest.mark.parametrize` lists, and keeps
every string in them (positives and negatives alike): Python's scanner decides what each
should find, so nothing here is labelled by hand.
"""

import importlib.util
import json
import sys
from pathlib import Path

HERE = Path(__file__).parent
# Tests of the ported checks and the parsing they rely on.
MODULES = [
    "test_side_channel",
    "test_side_channel_own_address",
    "test_outbound_credentials",
    "test_provider_keys",
    "test_webhook_credentials",
    "test_coding_agent_delegation",
    "test_credentials_context",
    "test_environment_dump",
    "test_harness_model_calls",
    "test_hub_access",
    "test_host_files",
    "test_installs",
    "test_lures",
    "test_shell",
    "test_tools",
]


def strings(value, out):
    if isinstance(value, str):
        if 3 <= len(value) <= 20_000:
            out.append(value)
    elif isinstance(value, (list, tuple, set, frozenset)):
        for item in value:
            strings(item, out)
    elif hasattr(value, "values") and hasattr(value, "id") and type(value).__name__ == "ParameterSet":
        strings(value.values, out)


def harvest(tests: Path) -> list[str]:
    sys.path.insert(0, str(tests))
    found: list[str] = []
    for name in MODULES:
        path = tests / f"{name}.py"
        if not path.exists():
            continue
        spec = importlib.util.spec_from_file_location(name, path)
        module = importlib.util.module_from_spec(spec)
        try:
            spec.loader.exec_module(module)
        except Exception as e:  # noqa: BLE001 - a test module that can't import is skipped
            print(f"skipped {name}: {e}")
            continue
        for item in vars(module).values():
            for mark in getattr(item, "pytestmark", []):
                if mark.name == "parametrize" and len(mark.args) >= 2:
                    strings(mark.args[1], found)
    return list(dict.fromkeys(found))


def main() -> None:
    found = harvest(Path(sys.argv[1]))
    cases = [{"name": "bash", "arguments": {"command": text}} for text in found]
    (HERE / "cases_harvested.json").write_text(json.dumps({"harvested": cases}, indent=1, ensure_ascii=False) + "\n")
    print(f"{len(cases)} strings")


if __name__ == "__main__":
    main()
