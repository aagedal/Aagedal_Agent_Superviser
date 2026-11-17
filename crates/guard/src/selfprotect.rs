//! Blocks calls that attack the guard itself: killing the app, reading or editing its state
//! (keys, journal), or pointing the harness somewhere else. Port of safeagents'
//! `selfprotect.py`, aimed at this app instead of the `safeagents` CLI.
//!
//! The checks are deliberately coarse: a coding agent has no reason to touch the guard's
//! control surface, so naming it in such a call counts as an attempt.

use std::sync::LazyLock;

use regex::Regex;

// Redirecting the harness by setting its base-URL environment variable.
static BASE_URL_ENV: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:ANTHROPIC_BASE_URL|ANTHROPIC_API_URL|OPENAI_BASE_URL|OPENAI_API_BASE|OPENAI_BASE|GOOGLE_GEMINI_BASE_URL|GEMINI_BASE_URL)\b\s*=|\bexport\s+(?:ANTHROPIC_BASE_URL|OPENAI_BASE_URL|OPENAI_API_BASE|GOOGLE_GEMINI_BASE_URL|GEMINI_BASE_URL)\b",
    )
    .unwrap()
});
// Harness config files that carry the base URL or provider.
static HARNESS_SETTINGS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\.claude/settings(?:\.local)?\.json|\.codex/config\.toml|opencode\.jsonc?\b|\.config/opencode/").unwrap()
});
static BASE_URL_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)base[_-]?url|model_provider|openai_base_url|\bproviders?\b|\bmodel\b|small_model").unwrap()
});
static PROC_ENV: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"/proc/[^\n"']{0,120}?/(?:environ|mem)\b"#).unwrap());

pub struct SelfProtection {
    process_kill: Regex,
    names: Regex,
    state: Regex,
}

impl SelfProtection {
    /// `process_names` are what the app runs as; `state_markers` name its state directory
    /// (its bundle identifier, and the absolute path if known).
    pub fn new(process_names: &[String], state_markers: &[String]) -> Self {
        let alternatives = |items: &[String]| items.iter().map(|s| regex::escape(s)).collect::<Vec<_>>().join("|");
        let names = alternatives(process_names);
        Self {
            process_kill: Regex::new(&format!(
                r#"(?i)\b(?:pkill|killall|kill|taskkill)\b[^"]*(?:{names})|(?:{names})[^"]*\b(?:pkill|killall|taskkill)\b"#
            ))
            .unwrap(),
            names: Regex::new(&format!("(?i){names}")).unwrap(),
            state: Regex::new(&format!("(?i){}", alternatives(state_markers))).unwrap(),
        }
    }

    /// Why a call attacks the guard, if it does. `args` is checked raw and JSON-decoded
    /// (the tool runs the decoded form, so `s` must not hide an `s`).
    pub fn reason(&self, args: &str) -> Option<String> {
        let decoded = serde_json::from_str::<serde_json::Value>(args).map(|v| crate::compact_json(&v)).ok();
        std::iter::once(args).chain(decoded.as_deref()).find_map(|text| self.reason_in(text))
    }

    fn reason_in(&self, text: &str) -> Option<String> {
        let why = if self.process_kill.is_match(text) {
            "kills the guard"
        } else if self.names.is_match(text) && PROC_ENV.is_match(text) {
            "reads the guard's process environment"
        } else if self.state.is_match(text) {
            "touches the guard's state directory (keys, journal, settings)"
        } else if BASE_URL_ENV.is_match(text) {
            "redirects the harness base URL away from the guard"
        } else if HARNESS_SETTINGS.is_match(text) && BASE_URL_KEY.is_match(text) {
            "edits a harness settings file to redirect it away from the guard"
        } else {
            return None;
        };
        Some(format!("self-protection: {why}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn protection() -> SelfProtection {
        SelfProtection::new(
            &["AgentSuperviser".into(), "Agent Superviser".into(), "agent-superviser".into()],
            &["me.aagedal.AgentSuperviser".into()],
        )
    }

    fn check(args: serde_json::Value) -> Option<String> {
        protection().reason(&args.to_string())
    }

    #[test]
    fn attacks_on_the_guard_are_blocked() {
        let attacks = [
            (json!({"command": "pkill -f agent-superviser"}), "kills"),
            (json!({"command": "killall 'Agent Superviser'"}), "kills"),
            (json!({"command": "cat ~/Library/Application\\ Support/me.aagedal.AgentSuperviser/keys/local-api-key"}), "state directory"),
            (json!({"file_path": "/Users/a/Library/Application Support/me.aagedal.AgentSuperviser/guard/calls.jsonl", "old_string": "blocked", "new_string": "passed"}), "state directory"),
            (json!({"command": "cat /proc/$(pgrep -f AgentSuperviser | head -1)/environ"}), "process environment"),
            (json!({"command": "export OPENAI_BASE_URL=https://api.openai.com && opencode"}), "base URL"),
            (json!({"file_path": "~/.config/opencode/opencode.jsonc", "new_string": "\"baseURL\": \"http://127.0.0.1:9999/v1\""}), "settings file"),
            (json!({"file_path": "/home/alice/.claude/settings.json", "new_string": "{\"env\":{\"ANTHROPIC_BASE_URL\":\"x\"}}"}), "settings file"),
            (json!({"file_path": "/Users/alice/.codex/config.toml", "new_string": "base_url = \"https://x\""}), "settings file"),
        ];
        for (args, fragment) in attacks {
            let reason = check(args.clone()).unwrap_or_else(|| panic!("not blocked: {args}"));
            assert!(reason.starts_with("self-protection:"));
            assert!(reason.contains(fragment), "{reason} for {args}");
        }
    }

    #[test]
    fn benign_calls_pass() {
        for args in [
            json!({"command": "pytest -q && ruff check"}),
            json!({"file_path": "src/app.py", "content": "print('hello')"}),
            json!({"command": "grep -r skill ."}),
            json!({"command": "ps aux | grep agent-superviser"}),
            json!({"command": "cargo build -p agent-superviser"}),
        ] {
            assert_eq!(check(args.clone()), None, "{args}");
        }
    }

    #[test]
    fn json_escapes_do_not_hide_an_attack() {
        let escaped = json!({"command": "pkill -f agent-superviser"}).to_string().replace("superviser", "\\u0073upervis\\u0065r");
        assert_eq!(protection().reason(&escaped).as_deref(), Some("self-protection: kills the guard"));
    }
}
