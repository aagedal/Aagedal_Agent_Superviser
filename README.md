# Agent Superviser

A menu bar app that makes local vibe-coding simple and safer:

1. Pick a model. Only models that fit your machine's memory are offered, always 4-bit (Q4_K_M or the closest variant) or better.
2. Start it. The app runs llama.cpp's `llama-server` behind a **guard** and sets up OpenCode to use it.
3. Code with OpenCode, or any OpenAI-compatible tool, or open the quick chat window.

The guard is a Rust port of Hugging Face's [safeagents](https://github.com/huggingface/safeagents). It checks every tool call the model makes before the harness runs it, and replaces blocked calls with a note. It is not a sandbox: it checks what the agent tries to do.

```
OpenCode / chat window ──▶ guard 127.0.0.1:8787/v1 ──▶ llama-server 127.0.0.1:<random>
     (local API key)          (checks tool calls)          (private key, only the guard has it)
```

## Status

Early. Working today:

- **Catalog and memory filter** (`crates/core`). The curated list lives in `catalog/curated.json`. Each model is pinned to a Hub commit with SHA-256 checksums and its KV-cache size, which is read from the GGUF header. Sliding-window layers are counted at their window size; the estimates match llama.cpp's allocations exactly for Qwen3.5 (hybrid) and Gemma 4 (sliding window). Models are offered at 64k context, or 32k if 64k doesn't fit, and never below 32k: OpenCode's system prompt alone is about 24k tokens. Files under 4 bits (Q1–Q3, IQ1–IQ3, TQ1) are never offered.
- **Downloads** from the Hub: resumable, checksum-verified, and gated by the same memory check for search results.
- **Models other apps already downloaded** are found and used in place: the Hugging Face cache (Unsloth Studio, `hf`, newer llama.cpp), LM Studio, Ollama, older llama.cpp `-hf` downloads and oMLX. Nothing is copied or deleted. They go through the same rules (4-bit or better, chat models only, memory fit). Drafts/MTP heads, vision projectors, embedding and speech models are skipped. MLX models are listed for later. A curated model already on disk in any quant shows **Start** instead of **Download**. Try `cargo run -p superviser-core --example discover`.
- **llama-server supervision**. The model gets the whole context in one slot. The key is passed via `--api-key-file`, so it never shows in `ps`.
- **Guard**: undeclared tools, hosts nobody mentioned, self-protection (killing the app, touching its keys or journal, redirecting the harness), streaming and whole responses, and a journal of flagged calls.
- **Scanner** (`crates/scan`): a Rust port of the 18 local-safety checks of atif-scan (safeagents' detector library) plus safeagents' own namespace check. They cover other agents' session history, API keys read or sent, other processes' environment or memory, network-isolation and egress-proxy bypass, credential-carrying requests and webhooks, calls to other models, delegation to `claude`/`codex`, and Hugging Face account recon. High findings block; the rest run and show as WARN. A parity test requires identical findings to Python safeagents on 1,615 calls, and the Rust shell reader matches Python's on all 1,573 corpus commands. Benchmark-integrity checks aren't ported (see `crates/scan/NOTICE.md`). About 0.3 ms per call.
- **Sandboxed OpenCode** (macOS, `crates/core/src/sandbox.rs`): "Code in OpenCode" opens a project in Terminal with OpenCode inside a Seatbelt sandbox that every command it runs inherits. Writes are allowed only in the project, temp folders and build caches. Credentials, keychains, browser data, shell history, other agents' sessions and the app's own keys can't be read. `open`, AppleScript, `launchctl`, the keychain CLI and Docker's socket are blocked. The environment is reduced to a few safe variables. Internet access is optional. A test checks the kernel actually enforces this on child processes.
- **OpenCode setup**. The app edits the global `opencode.json(c)` in place, keeping your comments, with a `.bak` backup. It adds a `superviser` provider, sets the default and title models, and restarts the 2.x service.
- **Menu bar app** (Tauri 2): start and stop models, an HF token stored in the keychain, the local API key, a live list of tool calls, and a chat window.

Not yet: the judge model for WARN calls, MLX, bundling `llama-server` in the `.app`, signing, Windows and Linux testing, and discrete-GPU VRAM detection.

## Develop

Needs Rust 1.90+ and `llama-server` on `PATH` (`brew install llama.cpp`), or `SUPERVISER_LLAMA_SERVER=/path/to/llama-server`.

```sh
cargo test --workspace
cargo run -p agent-superviser                         # the app (menu bar icon; opens Settings on first run)
cargo run -p superviser-core --example offers         # what each Mac memory size would be offered
cargo run -p superviser-core --example build_catalog  # regenerate catalog/curated.json from catalog/sources.json
cargo run -p superviser-core --example smoke -- unsloth/Qwen3.5-0.8B-GGUF /tmp/smoke   # download → llama-server → guard
```

App data lives in `~/Library/Application Support/me.aagedal.AgentSuperviser` on macOS: models, `keys/`, `guard/calls.jsonl` and `logs/`.

## Layout

| Path | What |
|---|---|
| `crates/core` | Catalog and memory fit, quant policy, GGUF header parsing, Hub client, installed models, llama-server, OpenCode config, secrets |
| `crates/guard` | The guard proxy (safeagents port; see `crates/guard/NOTICE.md`) |
| `crates/scan` | The scanner (atif-scan port; see `crates/scan/NOTICE.md`) and its parity harness |
| `app` | Tauri 2 menu bar app; `app/ui` is plain HTML/JS with no build step |
| `catalog` | `sources.json` (hand-edited) → `curated.json` (generated) |

## License

GPL-3.0-or-later. `crates/guard` contains code ported from safeagents (Apache-2.0); see its `NOTICE.md`.
