# Notice

This crate is a Rust port of parts of [safeagents](https://github.com/huggingface/safeagents),
Copyright 2026-present the HuggingFace Inc. team, licensed under the Apache License 2.0
(see `LICENSE-safeagents`). Ported from commit `dcf38e0a911d6e6a53966d46ecf1d9d6b86e3c01`.

| safeagents         | here              | changes                                                                      |
|--------------------|-------------------|------------------------------------------------------------------------------|
| `policy.py`, `constants.py` | `policy.rs` | Same rules. Regex lookarounds rewritten as explicit checks.                 |
| `selfprotect.py`   | `selfprotect.rs`  | Targets this app (process names, state directory) and OpenCode's config instead of the `safeagents` CLI. The atif-scan allowance rule is dropped. |
| `guard.py`         | `guard.rs`        | Runs `superviser-scan` (the atif-scan port) like safeagents runs atif-scan, inline (it takes about 0.3 ms). No sinks. Emits events for the app. |
| `judge.py`, judge constants | `judge.rs` | Asks a decision model through llama.cpp's `/v1/systemone` instead of a chat model, with the state and block question of the safeagents judge bench's `bench/decide.py` (the question's criteria dropped: llama.cpp takes none for yes/no questions). Blocks at P(block) ≥ 0.9. The app runs the model beside the chat model. |
| `rewrite.py`       | `rewrite.rs`      | Same behaviour. Notices say `[guard]`.                                       |
| `proxy.py`         | `proxy.rs`        | Adds a client API key, a separate upstream key, a switchable upstream and CORS for the app's webviews. |
| `journal.py`       | `journal.rs`      | Same format.                                                                 |

The tests in each module are ports of safeagents' own tests where they apply.
