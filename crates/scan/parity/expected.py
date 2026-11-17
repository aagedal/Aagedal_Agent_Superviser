"""Runs safeagents' scanner over data/corpus.jsonl and writes data/expected.jsonl."""

import json
from pathlib import Path

from atif_scan.data import shell
from safeagents.scan import Scanner, history, recent

HERE = Path(__file__).parent


def main() -> None:
    scanner = Scanner()
    corpus = HERE / "data" / "corpus.jsonl"
    out = HERE / "data" / "expected.jsonl"
    with corpus.open() as rows, out.open("w") as f:
        for line in rows:
            row = json.loads(line)
            signals = scanner.signals(recent(history(row["request"])), [(row["name"], row["arguments"])])
            findings = sorted({(s.id, s.severity.name.lower()) for s in signals})
            f.write(json.dumps({"id": row["id"], "findings": [{"id": i, "severity": s} for i, s in findings]}) + "\n")
    print(f"-> {out}")
    shells(corpus, HERE / "data" / "shell.jsonl")


def shells(corpus: Path, out: Path) -> None:
    """How atif-scan's shell reader reads every command string in the corpus."""
    seen = set()
    with corpus.open() as rows, out.open("w") as f:
        for line in rows:
            row = json.loads(line)
            try:
                args = json.loads(row["arguments"])
            except ValueError:
                continue
            command = args.get("command") if isinstance(args, dict) else None
            if not isinstance(command, str) or command in seen:
                continue
            seen.add(command)
            script = shell.parse(command)
            f.write(
                json.dumps(
                    {
                        "text": command,
                        "complete": script.complete,
                        "names": [c.name for c in script.commands],
                        "background": [c.background for c in script.commands],
                        "repeated": [c.repeated for c in script.commands],
                        "bodies": [command[s:e] for s, e in script.bodies],
                        "file_targets": script.file_targets,
                        "unused_file_bodies": [command[s:e] for s, e in shell.unused_file_bodies(script)],
                        "literal_search_patterns": [command[s:e] for s, e in shell.literal_search_patterns(command)],
                    },
                    ensure_ascii=False,
                )
                + "\n"
            )


if __name__ == "__main__":
    main()
