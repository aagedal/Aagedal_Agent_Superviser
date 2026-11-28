# Agent Superviser

A menu bar app that makes local vibe-coding simple and safer:

1. Pick a model. A short list suits your machine (most capable, fast, lighter, basic tasks), and only models that fit its memory are offered, always 4-bit (Q4_K_M or the closest variant) or better.
2. Pick a project folder. The app runs llama.cpp's `llama-server` behind a **guard**, and the agent works on the project in a sandbox.
3. Chat with **[Entropy](https://github.com/aagedal/entropy)**, Agent Superviser's own lean built-in coding harness (also available standalone as a TUI), or connect external harnesses like Pi, OpenCode, Codex CLI, or any OpenAI-compatible tool over the guarded local API.

The guard is a Rust port of Hugging Face's [safeagents](https://github.com/huggingface/safeagents). It checks every tool call the model makes before the harness runs it, and replaces blocked calls with a note. It is not a sandbox: it checks what the agent tries to do.

```
Entropy / Pi / OpenCode ──▶ guard 127.0.0.1:8787/v1 ──▶ llama-server 127.0.0.1:<random>
     (local API key)          (checks tool calls)          (private key, only the guard has it)
```

## Status

Early. Working today:

- **Catalog and memory filter** (`crates/core`). The curated list lives in `catalog/curated.json`. Each model is pinned to a Hub commit with SHA-256 checksums and its KV-cache size, which is read from the GGUF header. Sliding-window layers are counted at their window size; the estimates match llama.cpp's allocations exactly for Qwen3.5 (hybrid) and Gemma 4 (sliding window). Models are offered at 64k context, or 32k if 64k doesn't fit, and never below 32k: OpenCode's system prompt alone is about 24k tokens. The KV cache is 8-bit (`q8_0`), which takes 17/32 of the 16-bit size, so more models get the full context and more fit at once; reading a prompt is about 7% slower (Gemma 3 4B, 24k tokens). A model that won't load with it loads with a 16-bit cache, at the context that then fits. Files under 4 bits (Q1–Q3, IQ1–IQ3, TQ1) are never offered. Settings shows a short list first (`Catalog::picks`): the most capable model that fits, the most capable fast one, a lighter one and one for basic tasks, from hand-set ratings and each model's active parameters in `catalog/sources.json`. The rest are a click away.
- **Downloads** from the Hub: resumable, checksum-verified, and gated by the same memory check for search results. A download that wouldn't fit on the disk is refused before it starts; on macOS free space counts what the system frees on demand, as Finder does.
- **Models other apps already downloaded** are found and used in place: the Hugging Face cache (Unsloth Studio, `hf`, newer llama.cpp), LM Studio, Ollama, older llama.cpp `-hf` downloads and oMLX. Nothing is copied or deleted. They go through the same rules (4-bit or better, chat models only, memory fit). Drafts/MTP heads, vision projectors, embedding and speech models are skipped. MLX models are listed for later. A curated model already on disk in any quant shows **Start** instead of **Download**. Try `cargo run -p superviser-core --example discover`.
- **llama-server supervision**. The app bundles llama.cpp's `llama-server` (release b12631, pinned in `app/sidecars.json` with its libraries), so nothing needs installing. Several models can be loaded at once, each in its own llama-server, as long as they fit in memory together; loading one that doesn't fit offers to unload the ones used longest ago. The guard sends each request to the server of the model it names (others go to the most recently loaded), and `/v1/models` lists them all. Each model gets the whole context in one slot. Curated models run with the sampling their model card recommends for coding (in `catalog/sources.json`), as llama-server's defaults, since OpenCode sends none; samplers a card doesn't mention are off rather than llama.cpp's own top-k 40 and min-p 0.05. Models run with n-gram speculative decoding (`--spec-type ngram-mod`): coding agents re-emit much of what they read, and returning a 290-line file with one rename ran 6-7× faster (Gemma 4 12B, Qwen3.6 35B-A3B, Qwen3.8 27B on an M5 Pro) with identical output, while writing new code ran at the same speed. A model that won't load with it is loaded again without. Models whose card warns of reasoning loops get a thinking budget (Inkling 2 Mini: 16k tokens, `reasoning_budget` in `catalog/sources.json`); when it runs out, llama-server ends the thinking with a nudge to act. The key is passed via `--api-key-file`, so it never shows in `ps`. Conversations llama-server saves to RAM are capped at one at full context (its default is up to 8 GiB) and only use memory the model leaves free, so they never cost a model context. When a model is unloaded or the app quits, its last conversation is saved to disk (`crates/core/src/conversations.rs`, llama-server's slot save) and loaded back when the model starts, so an agent picking it up isn't kept waiting while the model reads it all again: a 24k-token prompt took 11 s to read, and 0.1 s after a restart (Gemma 3 4B, M5 Pro; 475 MB on disk). One per model, at most 10 GB in all and never more than a tenth of the free disk space, deleted after a week unused, when the model is deleted or when its file, cache type or context changes. They're in `~/Library/Caches/me.aagedal.AgentSuperviser`, which the sandbox hides from agents. Settings → Models → On this Mac shows what they take and can turn them off. Try `cargo run -p superviser-core --example resume -- model.gguf /tmp/resume`.
- **Guard**: undeclared tools, hosts nobody mentioned, self-protection (killing the app, touching its keys or journal, redirecting the harness), streaming and whole responses, and a journal of flagged calls. Local models sometimes write a tool call into their reply text instead of making it, so the agent never runs it and the turn just ends; the guard turns these back into real calls (`crates/guard/src/heal.rs`): Hermes/Qwen `<tool_call>{…}</tool_call>` JSON and Qwen3-Coder `<function=…><parameter=…>` XML, in OpenAI-format replies, whole or streamed. Only tools the request declared heal, and not when the tags are shown in code or the call was cut off. Healed calls go through the same checks as real ones. Streamed text is only held back from where a tag could be starting.
- **Judge** (`crates/guard/src/judge.rs`): optional and off by default. Turning it on in Settings → Safety downloads [d3-mini](https://huggingface.co/vllm-sr/d3-mini) (3.7 GB, Apache-2.0, a decision model built on Qwen3.5 4B, pinned in `catalog/curated.json`) and runs it in a second llama-server beside the model, through llama.cpp's `/v1/systemone` API. Calls the scanner marks WARN go to it with the user's messages, the call and the findings, never the agent's reasoning. It returns P(block); calls at 0.9 or above are blocked, and it can only add blocks. On the [safeagents judge bench](https://huggingface.co/datasets/harness-hub/safeagents-judge-bench) (661 real flagged calls) it blocks 92% of the bad calls and 6% of the legitimate ones at that threshold, in about 0.4 s each on an M5 Pro; asking the chat model instead (Qwen3.5 4B, safeagents' `judge.py` prompt) blocked 99% and 63%. The threshold was picked on the same calls, so expect somewhat worse elsewhere. It takes about 5.7 GB while running, which comes out of the models' budget: on machines without room for it beside any model it can't be turned on. If it doesn't answer within 30 s the call runs, unless Settings → Safety says to block it. Try `cargo run -p superviser-core --example judge -- vllm-d3-mini-Q4_K_M.gguf /tmp/judge`.
- **Scanner** (`crates/scan`): a Rust port of the 18 local-safety checks of atif-scan (safeagents' detector library) plus safeagents' own namespace check. They cover other agents' session history, API keys read or sent, other processes' environment or memory, network-isolation and egress-proxy bypass, credential-carrying requests and webhooks, calls to other models, delegation to `claude`/`codex`, and Hugging Face account recon. High findings block; the rest run and show as WARN. A parity test requires identical findings to Python safeagents on 1,615 calls, and the Rust shell reader matches Python's on all 1,573 corpus commands. Benchmark-integrity checks aren't ported (see `crates/scan/NOTICE.md`). About 0.3 ms per call.
- **Sandboxed OpenCode** (macOS, `crates/core/src/sandbox/`): "Open in OpenCode" opens a project in Terminal, isolated by a backend chosen per project. Backends implement one interface: availability, whether they suit a project, and launching. The default is a Seatbelt sandbox that every command OpenCode runs inherits; it works with the Mac's own tools. Writes are allowed only in the project, temp folders and build caches. Credentials, keychains, browser data, shell history, other agents' sessions and the app's own keys can't be read. `open`, AppleScript, `launchctl`, the keychain CLI and Docker's socket are blocked. The environment is reduced to a few safe variables. Internet access is optional. A test checks the kernel actually enforces this on child processes. The optional Linux VM backend runs OpenCode in a Linux VM per project (Apple `container`, macOS 26+, Apple silicon) that sees only the project folder; it turns away projects that need macOS tools (Xcode projects, CocoaPods, Swift packages for Apple platforms). Its image is built once per OpenCode version from `crates/core/src/sandbox/Containerfile`. The VM reaches the guard through the guard's owner-only Unix socket, forwarded by `container`; nothing listens on the network, so the macOS firewall never asks. When `container` is missing, the app links to Apple's signed installer.
- **Projects** (`crates/core/src/project.rs`): a main folder and related folders (read-only or writable), how it's isolated, internet, allowed websites, which agent tools are on, blocked programs, MCP servers turned off, and when the agent asks first. OpenCode gets them in `OPENCODE_CONFIG_CONTENT`, which beats a project's own `opencode.json`, and talks to the guard at `/projects/<id>/v1`, where the guard enforces them too (`crates/guard/src/project.rs`). Blocked programs also can't start in the macOS sandbox. Folders opened before there were projects became projects.
- **Entropy (Built-in Harness & Chat)** ([`aagedal/entropy`](https://github.com/aagedal/entropy), `crates/backend/src/agent.rs`, `app/ui/chat.html`, `app-gpui/src/chat.rs`): **Entropy**, Agent Superviser's standalone Rust coding harness, is the default chat engine—keeping base prompt + tool schema overhead under ~850 tokens so local models retain nearly the full 32k/64k context window. It combines a lean toolset (`read`, `glob`, `grep`, `write`, `edit`, `bash`, plus isolated read-only `subagent` routing to lighter secondary models when loaded) with forgiving multi-strategy edit matching (exact, indentation-normalized, whitespace-trimmed, block-anchor, atomic multi-hunk edits, closest-snippet hints, and concise unified diffs), automatic stale tool-output pruning and conversation compaction checkpoints, Pi-style mid-turn steering and chat branching (`fork`), `.entropy/SYSTEM.md` / `AGENTS.md` / `CLAUDE.md` support, `SKILL.md` skills, and pluggable sandboxing across both macOS Seatbelt and Apple `container` Linux VMs.
- **External Harnesses (Connect tab)** (`crates/core/src/connect.rs`): Settings → Connect provides live local API credentials and copyable setup instructions for **Pi** (`~/.pi/agent/models.json`), **OpenCode** (`opencode.json`), **Codex CLI** (`~/.codex/config.toml`), and any OpenAI-compatible client—all routed through the guard.
- **Menu bar app** (native GPUI in `app-gpui`, with the earlier Tauri 2 app in `app`): load and unload models, an HF token stored in the keychain, the local API key, a live list of tool calls, the chat window and the setup guide. While a model downloads, a chat's agent works, or for 5 minutes after any model call through the guard, the Mac doesn't go to sleep when left alone (`crates/core/src/awake.rs`; the display still may).
- **Releases**: `scripts/release-gpui-macos.sh` builds, signs, and notarizes the native GPUI app with bundled `llama.cpp` for Apple silicon (Intel and other platforms wait for a stable release), packages a signed & notarized DMG + `SHA256SUMS.txt`, and drafts a GitHub release (automatically marked as a prerelease for `-alpha`/`-beta`/`-rc` tags). An optional manual `.github/workflows/release.yml` workflow is also available if CI secrets are configured.

Not yet: MLX, auto-updates for the GPUI app, Windows and Linux testing, and discrete-GPU VRAM detection.

## Develop

Needs Rust 1.90+. Bundling the app downloads the llama.cpp release pinned in `app/sidecars.json` (checksum-verified) into `app/binaries/llama.cpp/`, via `scripts/fetch-llama-cpp.sh` (or `app/build.rs` for Tauri); see `app/NOTICE.md`. `SUPERVISER_LLAMA_SERVER=/path/to/llama-server` overrides the bundled `llama-server`; the examples, which don't bundle it, use the one on `PATH` (`brew install llama.cpp`).

```sh
cargo test --workspace
cargo run -p agent-superviser-gpui                    # native GPUI app (menu bar icon; opens Setup on first run)
./scripts/bundle-gpui-macos.sh --release              # package native macOS Agent Superviser.app with bundled llama.cpp
cargo run -p superviser-core --example offers         # what each Mac memory size would be offered
cargo run -p superviser-core --example build_catalog  # regenerate catalog/curated.json from catalog/sources.json
cargo run -p superviser-core --example smoke -- unsloth/Qwen3.5-0.8B-GGUF /tmp/smoke   # download → llama-server → guard
```

To develop against a local checkout of [`aagedal/entropy`](https://github.com/aagedal/entropy) without editing `Cargo.toml`, add a gitignored `.cargo/config.toml`:

```toml
[patch."https://github.com/aagedal/entropy"]
entropy = { path = "../entropy/crates/entropy" }
```

### Releases

Local signing & notarization (recommended — keeps your Developer ID certificate in your macOS Keychain):

1. Store your Apple notarization credentials once in Keychain:
   ```sh
   xcrun notarytool store-credentials "agent-superviser-notary" \
     --apple-id "you@example.com" \
     --team-id "TEAMID" \
     --password "xxxx-xxxx-xxxx-xxxx"
   ```
2. Set the version in `Cargo.toml`, `app-gpui/Info.plist` (`CFBundleShortVersionString`), and `app/tauri.conf.json`, commit, merge to `main`, and push the tag (`git tag v0.1.0-alpha.1 && git push origin v0.1.0-alpha.1`).
3. Build, sign, notarize, package the DMG + `SHA256SUMS.txt`, and create the draft GitHub release (automatically marked as a prerelease for tags containing `-`):
   ```sh
   ./scripts/release-gpui-macos.sh --notary-profile agent-superviser-notary --draft-release
   ```
4. Review the draft release on GitHub and click **Publish release**.

Optional CI release: `.github/workflows/release.yml` can also be triggered manually (`workflow_dispatch`) for an existing tag once `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD`, and `APPLE_TEAM_ID` are set in repository Actions secrets.

App data lives in `~/Library/Application Support/me.aagedal.AgentSuperviser` on macOS: models, `keys/`, `guard/calls.jsonl` and `logs/`.

## Layout

| Path | What |
|---|---|
| `crates/core` | Catalog and memory fit, quant policy, GGUF header parsing, Hub client, installed models, llama-server, external harness guides, secrets |
| `crates/guard` | The guard proxy (safeagents port; see `crates/guard/NOTICE.md`) |
| `crates/scan` | The scanner (atif-scan port; see `crates/scan/NOTICE.md`) and its parity harness |
| `crates/backend` | Shared UI-agnostic application runtime (`App`, model lifecycle, downloads, judge, OpenCode ACP, built-in agent, event bus) |
| `app` | Tauri 2 menu bar app; `app/ui` is plain HTML/JS with no build step |
| `app-gpui` | Native GPUI menu bar app sharing `crates/backend` |
| `catalog` | `sources.json` (hand-edited) → `curated.json` (generated) |

## License

GPL-3.0-or-later. `crates/guard` contains code ported from safeagents (Apache-2.0); see its `NOTICE.md`.
