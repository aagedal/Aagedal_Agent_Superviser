# Bundled programs

The macOS app bundles three programs, fetched at build time by `build.rs` from the releases pinned (with SHA-256 checksums) in `sidecars.json`:

- **OpenCode** (github.com/anomalyco/opencode), MIT License, in `Contents/Resources/opencode`. The chat window runs it headless (`opencode acp`), and "Open in OpenCode" runs it in Terminal.
- **ripgrep** (github.com/BurntSushi/ripgrep), MIT License or the Unlicense. OpenCode searches files with it.
- **llama.cpp**'s `llama-server` and the libraries it links (github.com/ggml-org/llama.cpp), MIT License, its `LICENSE` beside them in `Contents/Resources/llama.cpp`. It runs the models.

All are unmodified. OpenCode keeps its authors' Developer ID signature. Signed releases sign ripgrep and llama.cpp's files with the app's, since their releases are only ad-hoc signed.
