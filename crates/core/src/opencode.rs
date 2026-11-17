//! Points OpenCode at the guard.
//!
//! We add one provider to the user's global config and make its model the default, editing
//! the JSONC in place so comments and everything else the user wrote stay. The file is
//! backed up first. The config's own key style decides which we write: OpenCode 2.x reads
//! the 1.x keys (`provider`, `options`, `npm`, `small_model`) too, so we only switch to the
//! 2.x keys when the file already uses them.

use std::path::PathBuf;

use anyhow::{Context, bail};
use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstInputValue, CstObject, CstRootNode};
use serde_json::{Value, json};

pub const PROVIDER_ID: &str = "superviser";
const PROVIDER_NAME: &str = "Agent Superviser (local, guarded)";
const MAX_OUTPUT: u64 = 16_384;

#[derive(Debug, Clone)]
pub struct Setup {
    /// The guard's OpenAI-compatible base URL, ending in `/v1`.
    pub base_url: String,
    pub api_key_file: PathBuf,
    pub models: Vec<SetupModel>,
    /// Alias of the model to make OpenCode's default (and title model).
    pub default_model: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SetupModel {
    pub alias: String,
    pub name: String,
    pub context: u64,
}

/// `$XDG_CONFIG_HOME/opencode` or `~/.config/opencode`, on every OS.
pub fn config_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg).join("opencode"));
    }
    directories::BaseDirs::new().map(|d| d.home_dir().join(".config").join("opencode"))
}

/// The global config file: an existing `opencode.jsonc` or `opencode.json`, else a new `opencode.json`.
pub fn config_path() -> Option<PathBuf> {
    let dir = config_dir()?;
    ["opencode.jsonc", "opencode.json"].iter().map(|n| dir.join(n)).find(|p| p.exists()).or(Some(dir.join("opencode.json")))
}

/// `opencode --version`, e.g. `v2.3.0`, if OpenCode is installed.
pub fn version() -> Option<String> {
    let out = std::process::Command::new("opencode").arg("--version").output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn is_v2(version: Option<&str>) -> bool {
    version.is_some_and(|v| v.trim_start_matches(['v', 'V']).starts_with('2'))
}

/// `text` with our provider added or replaced. Pure, so it can be tested on any input.
pub fn apply(text: &str, setup: &Setup) -> anyhow::Result<String> {
    let text = if text.trim().is_empty() { "{\n  \"$schema\": \"https://opencode.ai/config.json\"\n}\n" } else { text };
    let root = CstRootNode::parse(text, &ParseOptions::default()).map_err(|e| anyhow::anyhow!("{e}"))?;
    let Some(config) = root.object_value() else { bail!("the OpenCode config is not a JSON object") };
    let v2_keys = config.get("providers").is_some();

    let models: serde_json::Map<String, Value> = setup
        .models
        .iter()
        .map(|m| {
            let limit = json!({ "context": m.context, "output": MAX_OUTPUT.min(m.context / 4) });
            (m.alias.clone(), json!({ "name": m.name, "limit": limit }))
        })
        .collect();
    let key = format!("{{file:{}}}", setup.api_key_file.display());
    let provider = if v2_keys {
        json!({
            "name": PROVIDER_NAME,
            "package": "@opencode/ai/providers/openai-compatible",
            "settings": { "baseURL": setup.base_url, "apiKey": key },
            "models": models,
        })
    } else {
        json!({
            "name": PROVIDER_NAME,
            "npm": "@ai-sdk/openai-compatible",
            "options": { "baseURL": setup.base_url, "apiKey": key },
            "models": models,
        })
    };
    let providers = config.object_value_or_set(if v2_keys { "providers" } else { "provider" });
    set(&providers, PROVIDER_ID, provider);

    if let Some(default) = &setup.default_model {
        let model = Value::String(format!("{PROVIDER_ID}/{default}"));
        set(&config, "model", model.clone());
        // Titles are written by a second model; it must go through the guard too.
        if v2_keys {
            let title = config.object_value_or_set("agents").object_value_or_set("title");
            set(&title, "model", model);
        } else {
            set(&config, "small_model", model);
        }
    }
    Ok(root.to_string())
}

fn set(object: &CstObject, key: &str, value: Value) {
    let value = to_cst(value);
    match object.get(key) {
        Some(prop) => prop.set_value(value),
        None => {
            object.append(key, value);
        }
    }
}

fn to_cst(value: Value) -> CstInputValue {
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(b) => CstInputValue::Bool(b),
        Value::Number(n) => CstInputValue::Number(n.to_string()),
        Value::String(s) => CstInputValue::String(s),
        Value::Array(items) => CstInputValue::Array(items.into_iter().map(to_cst).collect()),
        Value::Object(map) => CstInputValue::Object(map.into_iter().map(|(k, v)| (k, to_cst(v))).collect()),
    }
}

/// What [`install`] did.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Installed {
    pub path: PathBuf,
    pub backup: Option<PathBuf>,
    pub version: Option<String>,
    pub restarted_service: bool,
}

/// Writes the setup into the global config (after a `.bak` copy) and, on OpenCode 2.x,
/// restarts its background service so the change applies to the next session.
pub fn install(setup: &Setup) -> anyhow::Result<Installed> {
    let path = config_path().context("no home directory")?;
    let original = std::fs::read_to_string(&path).unwrap_or_default();
    let updated = apply(&original, setup).with_context(|| format!("updating {}", path.display()))?;
    let backup = if original.is_empty() {
        None
    } else {
        let backup = path.with_extension(format!("{}.bak", path.extension().and_then(|e| e.to_str()).unwrap_or("json")));
        std::fs::write(&backup, &original)?;
        Some(backup)
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if updated != original {
        std::fs::write(&path, updated)?;
    }
    let version = version();
    let restarted_service = is_v2(version.as_deref())
        && std::process::Command::new("opencode")
            .args(["service", "restart"])
            .output()
            .is_ok_and(|out| out.status.success());
    Ok(Installed { path, backup, version, restarted_service })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Setup {
        Setup {
            base_url: "http://127.0.0.1:8787/v1".into(),
            api_key_file: "/data/keys/local-api-key".into(),
            models: vec![SetupModel { alias: "qwen3.5-9b".into(), name: "Qwen3.5 9B".into(), context: 65_536 }],
            default_model: Some("qwen3.5-9b".into()),
        }
    }

    fn parse(text: &str) -> Value {
        jsonc_parser::parse_to_serde_value(text, &ParseOptions::default()).unwrap()
    }

    #[test]
    fn keeps_comments_and_other_providers() {
        let original = r#"{
  // my local setup
  "$schema": "https://opencode.ai/config.json",
  "provider": {
    "unsloth": { "npm": "@ai-sdk/openai-compatible", "options": { "baseURL": "http://127.0.0.1:8888/v1" } }
  },
  "mcp": { "x": { "type": "local", "command": ["x"] } }
}"#;
        let updated = apply(original, &setup()).unwrap();
        assert!(updated.contains("// my local setup"));
        let value = parse(&updated);
        assert!(value["provider"]["unsloth"].is_object());
        assert!(value["mcp"]["x"].is_object());
        let ours = &value["provider"][PROVIDER_ID];
        assert_eq!(ours["options"]["baseURL"], "http://127.0.0.1:8787/v1");
        assert_eq!(ours["options"]["apiKey"], "{file:/data/keys/local-api-key}");
        assert_eq!(ours["models"]["qwen3.5-9b"]["limit"]["context"], 65_536);
        assert_eq!(value["model"], "superviser/qwen3.5-9b");
        assert_eq!(value["small_model"], "superviser/qwen3.5-9b");
    }

    #[test]
    fn uses_v2_keys_when_the_file_does() {
        let original = r#"{ "providers": { "anthropic": { "settings": {} } }, "agents": { "title": { "model": "anthropic/claude-haiku-5-5" } } }"#;
        let value = parse(&apply(original, &setup()).unwrap());
        let ours = &value["providers"][PROVIDER_ID];
        assert_eq!(ours["package"], "@opencode/ai/providers/openai-compatible");
        assert_eq!(ours["settings"]["baseURL"], "http://127.0.0.1:8787/v1");
        assert_eq!(value["agents"]["title"]["model"], "superviser/qwen3.5-9b");
        assert!(value.get("provider").is_none());
        assert!(value.get("small_model").is_none());
    }

    #[test]
    fn is_idempotent_and_handles_empty_files() {
        let once = apply("", &setup()).unwrap();
        let twice = apply(&once, &setup()).unwrap();
        assert_eq!(once, twice);
        assert_eq!(parse(&once)["$schema"], "https://opencode.ai/config.json");
    }

    #[test]
    fn without_a_default_leaves_the_model_alone() {
        let mut s = setup();
        s.default_model = None;
        let value = parse(&apply(r#"{"model": "anthropic/x"}"#, &s).unwrap());
        assert_eq!(value["model"], "anthropic/x");
    }
}
