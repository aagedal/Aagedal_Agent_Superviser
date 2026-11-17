# Notice

This crate is a Rust port of parts of [safeagents](https://github.com/huggingface/safeagents),
Copyright 2026-present the HuggingFace Inc. team, licensed under the Apache License 2.0
(see `LICENSE-safeagents`). Ported from commit `dcf38e0a911d6e6a53966d46ecf1d9d6b86e3c01`.

| safeagents         | here              | changes                                                                      |
|--------------------|-------------------|------------------------------------------------------------------------------|
| `policy.py`, `constants.py` | `policy.rs` | Same rules. Regex lookarounds rewritten as explicit checks.                 |
| `selfprotect.py`   | `selfprotect.rs`  | Targets this app (process names, state directory) and OpenCode's config instead of the `safeagents` CLI. The atif-scan allowance rule is dropped. |
| `guard.py`         | `guard.rs`        | Synchronous. No atif-scan scanner, judge or sinks yet. Emits events for the app. |
| `rewrite.py`       | `rewrite.rs`      | Same behaviour. Notices say `[guard]`.                                       |
| `proxy.py`         | `proxy.rs`        | Adds a client API key, a separate upstream key, a switchable upstream and CORS for the app's webviews. |
| `journal.py`       | `journal.rs`      | Same format.                                                                 |

The tests in each module are ports of safeagents' own tests where they apply.
