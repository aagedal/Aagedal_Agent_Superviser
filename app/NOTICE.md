# Bundled programs

The macOS app bundles two programs, fetched at build time by `build.rs` from the releases pinned (with SHA-256 checksums) in `sidecars.json`:

- **OpenCode** (github.com/anomalyco/opencode), MIT License. The chat window runs it headless (`opencode acp`), and "Open in OpenCode" runs it in Terminal.
- **ripgrep** (github.com/BurntSushi/ripgrep), MIT License or the Unlicense. OpenCode searches files with it.

Both are unmodified and keep their authors' signatures.
