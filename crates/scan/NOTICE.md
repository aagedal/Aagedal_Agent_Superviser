# Notice

This crate ports parts of [atif-scan](https://github.com/huggingface/atif-scan), Copyright (c)
2026 Hugging Face, licensed under the MIT License (see `LICENSE-atif-scan`). Ported from commit
`3bbaf3c0019b548a9ff7b630ec03245361216701`, as [safeagents](https://github.com/huggingface/safeagents)
(`dcf38e0`) runs it on a pending tool call.

## Scope

atif-scan reviews whole agent trajectories, mostly for benchmark integrity (reward hacking,
verifier tampering, solution lookups). This crate ports only the checks that matter for a person
coding locally, and only what decides a match on the pending call (not coverage).

| atif-scan | here |
|---|---|
| `data/model.py`, `data/loader.py` (calls, fields) | `model.rs`, `trace.rs` |
| safeagents `scan.history`/`recent`/`call_step` | `trace.rs` |
| `data/tools.py` | `tools.rs`, without tool programs (`jslit`, `pylit`) |
| `data/shell.py` | `shell.rs`, arena instead of shared objects; byte spans |
| `data/credentials.py`, `data/webhooks.py` | `credentials.rs`, `webhooks.rs` |
| `data/outbound_*.py` | `outbound.rs` |
| `ast.parse` on untrusted Python | `pysrc.rs`, Ruff's parser (`ruff_python_parser`, MIT) |
| Python's `ipaddress` classes (3.13) | `net.rs` |
| `detectors/text.py` (`SurfaceDetector`, `RegexDetector`) | `detectors/mod.rs` |
| `detectors/side_channel.py` | `detectors/side_channel.rs`, `detectors/model_calls.rs` |
| `detectors/provider_keys.py` | `detectors/model_calls.rs` |
| `detectors/delegation.py`, `host_files.py`, `hub_access.py` | `detectors/agents.rs` |
| `detectors/outbound_credentials.py` | `detectors/outbound.rs` |

Ported checks (19): `access.harness_credentials`, `observation.credentials_exposed`,
`side_channel.fetch_proxy`, `side_channel.harness_introspection`,
`side_channel.harness_secret_read`, `side_channel.sandbox_process_memory`,
`side_channel.agent_history_probe`, `network.isolation_bypass`, `network.proxy_bypass`,
`side_channel.model_call`, `side_channel.provider_credential_use`,
`side_channel.harness_key_model_call`, `side_channel.coding_agent_delegation`,
`side_channel.host_file_upload`, `side_channel.hub_identity_request`,
`side_channel.hub_account_reconnaissance`, `side_channel.outbound_credential`,
`side_channel.webhook_use`, and safeagents' `safeagents.namespace_escape`.

Not ported:

- Benchmark-integrity checks (`lookup.*`, `tamper.*`, `access.test_path`,
  `access.evaluation_*`, `access.verifier_logs`, `harness.*`,
  `side_channel.memory_grader_search`, `side_channel.host_solution_upload`).
- Install lures (`side_channel.lure_pursued`, `side_channel.install_lure_followed`): they
  recognise one known campaign against Terminal-Bench, and otherwise only scripts named after
  a benchmark or the trace's task, which a guard never knows.
- `side_channel.hub_account_content_received`: it needs the call's own result, which a
  pending call doesn't have.
- Trace-quality and informational checks (`integrity.*`, `awareness.*`, `recall.*`, info
  severity), and allowances (atif-scan's built-in ones only excuse benchmark wording).
- Codex's open-by-reference web tool and tool programs (Codex code mode, fast-agent
  `execute_python`): their calls are read as plain tool calls.

`parity/` holds the corpus and Python's findings that `tests/parity.rs` compares against.
