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
const COMMAND_KEYS: &[&str] = &["command", "cmd", "script"];

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
                Some(Value::Array(words)) => words.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "),
                _ => continue,
            };
            // Nested commands (`sh -c '…'`, substitutions) are listed too.
            let script = shell::parse(&text);
            if let Some(program) = script.commands().filter_map(|c| c.name()).find(|n| self.blocked_programs.iter().any(|p| p == n)) {
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
    }

    #[test]
    fn splits_project_paths() {
        assert_eq!(split_path("/projects/ab12/v1/chat/completions"), Some(("ab12", "/v1/chat/completions".into())));
        assert_eq!(split_path("/projects/ab12?x=1"), Some(("ab12", "/?x=1".into())));
        assert_eq!(split_path("/projects/"), None);
        assert_eq!(split_path("/v1/models"), None);
    }
}
