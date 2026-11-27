//! Setup instructions and ready-to-paste config snippets for external coding harnesses
//! (Pi, OpenCode, Codex CLI, and any OpenAI-compatible client) connecting to Agent Superviser's
//! guarded local API.

use serde::Serialize;
use serde_json::json;

/// One ready-to-paste setup guide shown in the Connect tab.
#[derive(Debug, Clone, Serialize)]
pub struct HarnessGuide {
    pub id: &'static str,
    pub name: &'static str,
    pub subtitle: &'static str,
    pub file_path: &'static str,
    pub snippet: String,
    pub notes: String,
}

/// Parameters describing the live guarded local API endpoint.
#[derive(Debug, Clone)]
pub struct ConnectParams<'a> {
    pub base_url: &'a str,
    pub api_key: &'a str,
    pub model_alias: &'a str,
    pub model_name: &'a str,
    pub context_window: u64,
}

/// Generates copyable setup guides for popular external harnesses.
pub fn guides(params: &ConnectParams<'_>) -> Vec<HarnessGuide> {
    let base_url = if params.base_url.is_empty() {
        "http://127.0.0.1:8787/v1"
    } else {
        params.base_url
    };
    let model = if params.model_alias.is_empty() {
        "<loaded-model>"
    } else {
        params.model_alias
    };
    let display_name = if params.model_name.is_empty() {
        model
    } else {
        params.model_name
    };
    let ctx = params.context_window.max(32_768);
    let max_out = 16_384u64.min(ctx / 4);

    let pi_models = serde_json::to_string_pretty(&json!({
        "providers": {
            "superviser": {
                "baseUrl": base_url,
                "api": "openai-completions",
                "apiKey": params.api_key,
                "models": [
                    {
                        "id": model,
                        "name": display_name,
                        "contextWindow": ctx,
                        "maxTokens": max_out
                    }
                ]
            }
        }
    }))
    .unwrap_or_default();

    let opencode_json = serde_json::to_string_pretty(&json!({
        "$schema": "https://opencode.ai/config.json",
        "provider": {
            "superviser": {
                "npm": "@ai-sdk/openai-compatible",
                "name": "Agent Superviser (local, guarded)",
                "options": {
                    "baseURL": base_url,
                    "apiKey": params.api_key
                },
                "models": {
                    model: {
                        "name": display_name,
                        "limit": { "context": ctx, "output": max_out }
                    }
                }
            }
        },
        "model": format!("superviser/{model}"),
        "small_model": format!("superviser/{model}")
    }))
    .unwrap_or_default();

    let codex_toml = format!(
        "model_provider = \"superviser\"\n\
         model = \"{model}\"\n\n\
         [model_providers.superviser]\n\
         name = \"Agent Superviser (local, guarded)\"\n\
         base_url = \"{base_url}\"\n\
         env_key = \"SUPERVISER_API_KEY\"\n\
         wire_api = \"chat\"\n"
    );

    let env_snippet = format!(
        "export OPENAI_BASE_URL=\"{base_url}\"\n\
         export OPENAI_API_KEY=\"{key}\"\n\
         export SUPERVISER_API_KEY=\"{key}\"\n\
         # Model id: {model}",
        key = params.api_key
    );

    vec![
        HarnessGuide {
            id: "pi",
            name: "Pi (pi-coding-agent)",
            subtitle: "Minimalist terminal harness (~1k-token prompt, great for local models)",
            file_path: "~/.pi/agent/models.json",
            snippet: pi_models,
            notes: format!(
                "Save to ~/.pi/agent/models.json, then run `pi` and pick `{model}` via /model (or set defaultProvider=\"superviser\" and defaultModel=\"{model}\" in ~/.pi/agent/settings.json)."
            ),
        },
        HarnessGuide {
            id: "opencode",
            name: "OpenCode",
            subtitle: "Full-featured terminal coding agent (uses ~24k tokens for its system prompt)",
            file_path: "~/.config/opencode/opencode.json",
            snippet: opencode_json,
            notes: "Merge into ~/.config/opencode/opencode.json (or opencode.jsonc). Every tool call passes through Agent Superviser's guard.".into(),
        },
        HarnessGuide {
            id: "codex",
            name: "Codex CLI",
            subtitle: "OpenAI Codex CLI pointed at your local guarded endpoint",
            file_path: "~/.codex/config.toml",
            snippet: codex_toml,
            notes: "Add to ~/.codex/config.toml and export SUPERVISER_API_KEY with the API key above before launching `codex`.".into(),
        },
        HarnessGuide {
            id: "openai",
            name: "Any OpenAI-compatible tool",
            subtitle: "Standard environment variables for CLIs, editors, and scripts",
            file_path: "Shell environment",
            snippet: env_snippet,
            notes: "Works with any tool supporting OpenAI Chat Completions (/v1/chat/completions) and /v1/models.".into(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_guides_for_pi_opencode_codex_and_openai() {
        let list = guides(&ConnectParams {
            base_url: "http://127.0.0.1:8787/v1",
            api_key: "sk-test",
            model_alias: "qwen3-27b",
            model_name: "Qwen3.8 27B",
            context_window: 65_536,
        });
        let ids: Vec<&str> = list.iter().map(|g| g.id).collect();
        assert_eq!(ids, ["pi", "opencode", "codex", "openai"]);
        assert!(list[0].snippet.contains("\"baseUrl\": \"http://127.0.0.1:8787/v1\""));
        assert!(list[0].snippet.contains("\"qwen3-27b\""));
        assert!(list[1].snippet.contains("\"superviser/qwen3-27b\""));
        assert!(list[2].snippet.contains("base_url = \"http://127.0.0.1:8787/v1\""));
        assert!(list[3].snippet.contains("OPENAI_BASE_URL=\"http://127.0.0.1:8787/v1\""));
    }
}
