//! API keys and the Hugging Face token.
//!
//! The Hugging Face token lives in the OS keychain. The two local keys live in owner-only
//! files: OpenCode reads the harness key with `{file:…}`, and llama-server reads its key with
//! `--api-key-file`, so neither ever shows up in a process list.

use std::path::Path;

use crate::paths::{Paths, write_private};

const KEYRING_SERVICE: &str = "me.aagedal.AgentSuperviser";
const HF_TOKEN_USER: &str = "huggingface";

pub fn generate_key(prefix: &str) -> String {
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).expect("system randomness");
    format!("{prefix}{}", hex::encode(bytes))
}

/// The key in `path`, created on first use.
pub fn key_file(path: &Path, prefix: &str) -> anyhow::Result<String> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        let existing = existing.trim();
        if !existing.is_empty() {
            return Ok(existing.to_string());
        }
    }
    let key = generate_key(prefix);
    write_private(path, key.as_bytes())?;
    Ok(key)
}

pub fn local_api_key(paths: &Paths) -> anyhow::Result<String> {
    key_file(&paths.local_api_key(), "sk-local-")
}

pub fn regenerate_local_api_key(paths: &Paths) -> anyhow::Result<String> {
    let key = generate_key("sk-local-");
    write_private(&paths.local_api_key(), key.as_bytes())?;
    Ok(key)
}

pub fn upstream_api_key(paths: &Paths) -> anyhow::Result<String> {
    key_file(&paths.upstream_api_key(), "sk-upstream-")
}

fn hf_entry() -> keyring::Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, HF_TOKEN_USER)
}

pub fn hf_token() -> Option<String> {
    hf_entry().ok()?.get_password().ok().filter(|t| !t.is_empty())
}

pub fn set_hf_token(token: Option<&str>) -> anyhow::Result<()> {
    let entry = hf_entry()?;
    match token.map(str::trim).filter(|t| !t.is_empty()) {
        Some(token) => entry.set_password(token)?,
        None => match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(e) => return Err(e.into()),
        },
    }
    Ok(())
}
