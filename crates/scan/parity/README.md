# Parity with atif-scan

The Rust scanner must report the same findings as safeagents' Python scanner (atif-scan) for
every check it ports. `tests/parity.rs` checks that offline from two committed files:

- `data/corpus.jsonl`: one tool call per line, as the guard sees it: `request` (the
  conversation and declared tools), `name` and raw JSON `arguments`.
- `data/expected.jsonl`: what Python safeagents' scanner found for each call.
- `data/shell.jsonl`: how atif-scan's shell reader read every command in the corpus.

Regenerate them after changing `cases.json` or bumping the pinned versions (Python 3.13: its
`ipaddress` rules decide some network checks):

```sh
PINS='--python 3.13 --with pytest --with pyarrow
  --with atif-scan@git+https://github.com/huggingface/atif-scan@3bbaf3c0019b548a9ff7b630ec03245361216701
  --with safeagents@git+https://github.com/huggingface/safeagents@dcf38e0a911d6e6a53966d46ecf1d9d6b86e3c01'
git clone https://github.com/huggingface/atif-scan /tmp/atif-scan && git -C /tmp/atif-scan checkout 3bbaf3c
uv run $PINS python crates/scan/parity/harvest.py /tmp/atif-scan/tests
uv run $PINS python crates/scan/parity/build_corpus.py
uv run $PINS python crates/scan/parity/expected.py
```

Sources:

- `bench`: the 661 calls of [harness-hub/safeagents-judge-bench](https://huggingface.co/datasets/harness-hub/safeagents-judge-bench)
  (Apache-2.0), real agent calls the scanner found suspicious. Calls through fast-agent's
  Python-program tools are excluded: reading those programs (atif-scan's `pylit`) isn't ported.
- `cases`: `cases.json`, hand-written in OpenCode's tool shapes. `benign` are everyday coding
  calls that must stay clean, `attacks` single calls, and `conversations` calls after earlier
  turns (a key shown by `env`, then used; a `whoami`, then the account's repos).
- `harvested`: `cases_harvested.json`, every string in the `parametrize` lists of atif-scan's
  tests for the ported checks, each as a `bash` command. Nothing is labelled by hand: Python's
  scanner decides what each one should find.
