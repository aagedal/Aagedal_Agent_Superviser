"""Builds data/corpus.jsonl from the judge bench and cases.json."""

import json
import urllib.request
from pathlib import Path

import pyarrow.parquet as pq

HERE = Path(__file__).parent
BENCH = "https://huggingface.co/datasets/harness-hub/safeagents-judge-bench/resolve/main/data/test-00000-of-00001.parquet"
# Reading programs written for these tools (atif-scan's pylit) isn't ported.
PROGRAM_TOOLS = {"execute_python", "exec_sandbox"}


def request(task: str, tool: str, history: list | None = None) -> dict:
    """A request whose conversation is the task, then `history` (OpenAI messages)."""
    return {
        "messages": [{"role": "user", "content": task}, *(history or [])],
        "tools": [{"type": "function", "function": {"name": tool}}],
    }


def main() -> None:
    rows = []
    local = HERE / "bench.parquet"
    if not local.exists():
        urllib.request.urlretrieve(BENCH, local)
    for row in pq.read_table(local).to_pylist():
        if row["tool_name"] in PROGRAM_TOOLS:
            continue
        rows.append(
            {
                "id": f"bench/{row['id']}",
                "request": request(row["task_text"] or "", row["tool_name"]),
                "name": row["tool_name"],
                "arguments": row["tool_args"],
            }
        )
    cases = json.loads((HERE / "cases.json").read_text())
    cases.update(json.loads((HERE / "cases_harvested.json").read_text()))
    for group, items in cases.items():
        if group.startswith("_"):
            continue
        for index, case in enumerate(items):
            rows.append(
                {
                    "id": f"cases/{group}/{index}",
                    "request": request(case.get("task", "Help me with this project."), case["name"], case.get("history")),
                    "name": case["name"],
                    "arguments": json.dumps(case["arguments"]),
                }
            )
    out = HERE / "data" / "corpus.jsonl"
    with out.open("w") as f:
        for row in rows:
            f.write(json.dumps(row, ensure_ascii=False) + "\n")
    local.unlink()
    print(f"{len(rows)} calls -> {out}")


if __name__ == "__main__":
    main()
