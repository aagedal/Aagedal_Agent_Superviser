# Agent Superviser

A menu bar app that makes local vibe-coding simple and safer:

1. Pick a model. A short list suits your machine (most capable, fast, lighter, basic tasks), and only models that fit its memory are offered, always 4-bit (Q4_K_M or the closest variant) or better.
2. Pick a project folder. The app runs llama.cpp's `llama-server` behind a **guard**, and the agent works on the project in a sandbox.
3. Chat with the coding agent in the app's window, or open the project in OpenCode in Terminal, or use any OpenAI-compatible tool.

The guard is a Rust port of Hugging Face's [safeagents](https://github.com/huggingface/safeagents). It checks every tool call the model makes before the harness runs it, and replaces blocked calls with a note. It is not a sandbox: it checks what the agent tries to do.

```
OpenCode / chat window ──▶ guard 127.0.0.1:8787/v1 ──▶ llama-server 127.0.0.1:<random>
     (local API key)          (checks tool calls)          (private key, only the guard has it)
```

## Status

Early. Working today:

- **Catalog and memory filter** (`crates/core`). The curated list lives in `catalog/curated.json`. Each model is pinned to a Hub commit with SHA-256 checksums and its KV-cache size, which is read from the GGUF header. Sliding-window layers are counted at their window size; the estimates match llama.cpp's allocations exactly for Qwen3.5 (hybrid) and Gemma 4 (sliding window). Models are offered at 64k context, or 32k if 64k doesn't fit, and never below 32k: OpenCode's system prompt alone is about 24k tokens. Files under 4 bits (Q1–Q3, IQ1–IQ3, TQ1) are never offered. Settings shows a short list first (`Catalog::picks`): the most capable model that fits, the most capable fast one, a lighter one and one for basic tasks, from hand-set ratings and each model's active parameters in `catalog/sources.json`. The rest are a click away.
- **Downloads** from the Hub: resumable, checksum-verified, and gated by the same memory check for search results.
- **Models other apps already downloaded** are found and used in place: the Hugging Face cache (Unsloth Studio, `hf`, newer llama.cpp), LM Studio, Ollama, older llama.cpp `-hf` downloads and oMLX. Nothing is copied or deleted. They go through the same rules (4-bit or better, chat models only, memory fit). Drafts/MTP heads, vision projectors, embedding and speech models are skipped. MLX models are listed for later. A curated model already on disk in any quant shows **Start** instead of **Download**. Try `cargo run -p superviser-core --example discover`.
- **llama-server supervision**. The model gets the whole context in one slot. The key is passed via `--api-key-file`, so it never shows in `ps`. Conversations llama-server saves to RAM are capped at one at full context (its default is up to 8 GiB) and only use memory the model leaves free, so they never cost a model context.
- **Guard**: undeclared tools, hosts nobody mentioned, self-protection (killing the app, touching its keys or journal, redirecting the harness), streaming and whole responses, and a journal of flagged calls.
- **Judge** (`crates/guard/src/judge.rs`): optional and off by default. Turning it on in Settings → Safety downloads [d3-mini](https://huggingface.co/vllm-sr/d3-mini) (3.7 GB, Apache-2.0, a decision model built on Qwen3.5 4B, pinned in `catalog/curated.json`) and runs it in a second llama-server beside the model, through llama.cpp's `/v1/systemone` API. Calls the scanner marks WARN go to it with the user's messages, the call and the findings, never the agent's reasoning. It returns P(block); calls at 0.9 or above are blocked, and it can only add blocks. On the [safeagents judge bench](https://huggingface.co/datasets/harness-hub/safeagents-judge-bench) (661 real flagged calls) it blocks 92% of the bad calls and 6% of the legitimate ones at that threshold, in about 0.4 s each on an M5 Pro; asking the chat model instead (Qwen3.5 4B, safeagents' `judge.py` prompt) blocked 99% and 63%. The threshold was picked on the same calls, so expect somewhat worse elsewhere. It takes about 5.7 GB while running, which comes out of the models' budget: on machines without room for it beside any model it can't be turned on. If it doesn't answer within 30 s the call runs, unless Settings → Safety says to block it. Try `cargo run -p superviser-core --example judge -- vllm-d3-mini-Q4_K_M.gguf /tmp/judge`.
- **Scanner** (`crates/scan`): a Rust port of the 18 local-safety checks of atif-scan (safeagents' detector library) plus safeagents' own namespace check. They cover other agents' session history, API keys read or sent, other processes' environment or memory, network-isolation and egress-proxy bypass, credential-carrying requests and webhooks, calls to other models, delegation to `claude`/`codex`, and Hugging Face account recon. High findings block; the rest run and show as WARN. A parity test requires identical findings to Python safeagents on 1,615 calls, and the Rust shell reader matches Python's on all 1,573 corpus commands. Benchmark-integrity checks aren't ported (see `crates/scan/NOTICE.md`). About 0.3 ms per call.
- **Sandboxed OpenCode** (macOS, `crates/core/src/sandbox/`): "Open in OpenCode" opens a project in Terminal, isolated by a backend chosen per project. Backends implement one interface: availability, whether they suit a project, and launching. The default is a Seatbelt sandbox that every command OpenCode runs inherits; it works with the Mac's own tools. Writes are allowed only in the project, temp folders and build caches. Credentials, keychains, browser data, shell history, other agents' sessions and the app's own keys can't be read. `open`, AppleScript, `launchctl`, the keychain CLI and Docker's socket are blocked. The environment is reduced to a few safe variables. Internet access is optional. A test checks the kernel actually enforces this on child processes. The optional Linux VM backend runs OpenCode in a Linux VM per project (Apple `container`, macOS 26+, Apple silicon) that sees only the project folder; it turns away projects that need macOS tools (Xcode projects, CocoaPods, Swift packages for Apple platforms). Its image is built once per OpenCode version from `crates/core/src/sandbox/Containerfile`. The VM reaches the guard through the guard's owner-only Unix socket, forwarded by `container`; nothing listens on the network, so the macOS firewall never asks. When `container` is missing, the app links to Apple's signed installer.
- **Projects** (`crates/core/src/project.rs`): a main folder and related folders (read-only or writable), how it's isolated, internet, allowed websites, which agent tools are on, blocked programs, MCP servers turned off, and when the agent asks first. OpenCode gets them in `OPENCODE_CONFIG_CONTENT`, which beats a project's own `opencode.json`, and talks to the guard at `/projects/<id>/v1`, where the guard enforces them too (`crates/guard/src/project.rs`). Blocked programs also can't start in the macOS sandbox. Folders opened before there were projects became projects.
- **Chat** (`app/src/acp.rs`, `app/ui/chat.html`): the coding agent in a window, no terminal. The app runs OpenCode headless (`opencode acp`, the Agent Client Protocol), one per project, in the project's macOS sandbox, with our provider as the only one enabled. The window shows its replies, tool calls, diffs and command output, and its permission requests become Allow/Deny buttons; chats are OpenCode's saved sessions. Without OpenCode, a small built-in agent (`crates/core/src/agent/`, `app/src/agent.rs`) does the same with its own tools. Linux VM projects open in OpenCode in Terminal for now.
- **Setup guide**: on first run, Settings walks through picking a model, a first project and the safety options, then opens the chat.
- **OpenCode setup**. The app edits the global `opencode.json(c)` in place, keeping your comments, with a `.bak` backup. It adds a `superviser` provider, sets the default and title models, and restarts the 2.x service.
- **Menu bar app** (Tauri 2): start and stop models, an HF token stored in the keychain, the local API key, a live list of tool calls, the chat window and the setup guide.

Not yet: MLX, the chat for Linux VM projects, bundling `llama-server` in the `.app`, signing, Windows and Linux testing, and discrete-GPU VRAM detection.

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
