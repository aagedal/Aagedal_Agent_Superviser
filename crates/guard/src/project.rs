//! What a project adds to the guard's checks: sites its calls may always reach, and tools and
//! programs it may not use. The app points OpenCode for a project at `/projects/<id>/v1`, so
//! the guard knows which project a request comes from.

use serde_json::Value;
use superviser_scan::shell;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProjectRules {
    /// What the app calls the project, for block reasons and the activity view.
    pub name: String,
    /// Hosts its calls may reach even if the conversation never mentions them.
    pub allowed_hosts: Vec<String>,
    /// Tool names it may not call.
    pub blocked_tools: Vec<String>,
    /// Programs its shell commands may not run, by name (`ssh`, `docker`).
    pub blocked_programs: Vec<String>,
}

/// Argument keys that hold a shell command.
const COMMAND_KEYS: &[&str] = &["command", "cmd", "script", "shell_command", "bash", "commands", "chars", "code"];

fn shell_quote_word(word: &str) -> String {
    if word.is_empty() {
        return "''".into();
    }
    if word.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'-' | b'_' | b':' | b'=' | b'@')) {
        return word.to_string();
    }
    format!("'{}'", word.replace('\'', r"'\''"))
}

fn contains_word_ci(text: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let hay = text.to_ascii_lowercase();
    let pat = needle.to_ascii_lowercase();
    let bytes = hay.as_bytes();
    let mut from = 0;
    while let Some(pos) = hay[from..].find(&pat) {
        let start = from + pos;
        let end = start + pat.len();
        let before_ok = start == 0 || !(bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_' || bytes[start - 1] == b'-');
        let after_ok = end == bytes.len() || !(bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_' || bytes[end] == b'-');
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

impl ProjectRules {
    /// Why the call breaks this project's rules, if it does.
    pub fn check(&self, name: &str, args: &str) -> Option<String> {
        if self.blocked_tools.iter().any(|t| t.eq_ignore_ascii_case(name)) {
            return Some(format!("`{name}` is turned off for project {}", self.name));
        }
        if self.blocked_programs.is_empty() {
            return None;
        }
        let Ok(Value::Object(args)) = serde_json::from_str::<Value>(args) else { return None };
        for key in COMMAND_KEYS {
            let text = match args.get(*key) {
                Some(Value::String(text)) => text.clone(),
                Some(Value::Array(words)) => {
                    let strs: Vec<&str> = words.iter().filter_map(Value::as_str).collect();
                    if let Some(first) = strs.first() {
                        let base = first.rsplit('/').next().unwrap_or(first);
                        if self.blocked_programs.iter().any(|p| p.eq_ignore_ascii_case(base)) {
                            return Some(format!("`{base}` is blocked for project {}", self.name));
                        }
                    }
                    strs.into_iter().map(shell_quote_word).collect::<Vec<_>>().join(" ")
                }
                _ => continue,
            };
            // Nested commands (`sh -c '…'`, substitutions) are listed too.
            let script = shell::parse(&text);
            if let Some(program) = script.commands().filter_map(|c| c.name()).find(|n| self.blocked_programs.iter().any(|p| p.eq_ignore_ascii_case(n))) {
                return Some(format!("`{program}` is blocked for project {}", self.name));
            }
            // If shell parsing bailed on an unsupported construct (e.g. `f() { ... }` or `case`),
            // fall back to a word-boundary check so blocked programs don't fail open.
            if !script.complete
                && let Some(program) = self.blocked_programs.iter().find(|p| contains_word_ci(&text, p))
            {
                return Some(format!("`{program}` is blocked for project {}", self.name));
            }
        }
        None
    }
}

/// `/projects/<id>/rest?query` → (`id`, `/rest?query`); `None` for other paths.
pub fn split_path(path_and_query: &str) -> Option<(&str, String)> {
    let rest = path_and_query.strip_prefix("/projects/")?;
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (id, tail) = rest.split_at(end);
    (!id.is_empty()).then(|| (id, if tail.starts_with('/') { tail.to_string() } else { format!("/{tail}") }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> ProjectRules {
        ProjectRules { name: "site".into(), allowed_hosts: vec![], blocked_tools: vec!["webfetch".into()], blocked_programs: vec!["ssh".into(), "docker".into()] }
    }

    #[test]
    fn blocks_turned_off_tools() {
        assert!(rules().check("webfetch", r#"{"url": "https://example.com"}"#).unwrap().contains("turned off for project site"));
        assert_eq!(rules().check("read", r#"{"filePath": "a.txt"}"#), None);
    }

    #[test]
    fn blocks_programs_anywhere_in_a_command() {
        let check = |command: &str| rules().check("bash", &serde_json::json!({ "command": command }).to_string());
        assert!(check("ssh host uptime").unwrap().contains("`ssh`"));
        assert!(check("cd app && /usr/bin/ssh host").is_some());
        assert!(check("sh -c 'docker run alpine'").unwrap().contains("`docker`"));
        assert!(check("echo $(ssh host cat /etc/hosts)").is_some());
        assert_eq!(check("git status && npm test"), None);
        // A program's name as an argument is not running it.
        assert_eq!(check("grep ssh notes.txt"), None);
        assert!(rules().check("exec_command", r#"{"cmd": ["bash", "-lc", "ssh host"]}"#).is_some());
        // Case-insensitive match and fallback when shell parsing encounters unsupported constructs.
        assert!(check("SSH host uptime").is_some());
        assert!(check("f() { ssh host; }; f").is_some());
        assert!(check("case x in x) ssh host;; esac").is_some());
        assert!(rules().check("exec_command", r#"{"cmd": ["bash", "-lc", "echo it's; ssh host"]}"#).is_some());
    }

    #[test]
    fn splits_project_paths() {
        assert_eq!(split_path("/projects/ab12/v1/chat/completions"), Some(("ab12", "/v1/chat/completions".into())));
        assert_eq!(split_path("/projects/ab12?x=1"), Some(("ab12", "/?x=1".into())));
        assert_eq!(split_path("/projects/"), None);
        assert_eq!(split_path("/v1/models"), None);
    }
}
