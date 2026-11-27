# Bundled programs

The macOS app bundles one external program, fetched at build time by `build.rs` from the release pinned (with SHA-256 checksums) in `sidecars.json`:

- **llama.cpp**'s `llama-server` and the libraries it links (github.com/ggml-org/llama.cpp), MIT License, its `LICENSE` beside them in `Contents/Resources/llama.cpp`. It runs the local models.

It is unmodified. Signed releases sign llama.cpp's files with the app's Developer ID certificate, since its upstream release is only ad-hoc signed.
