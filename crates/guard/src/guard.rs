//! Reviews the tool calls of one response to one request. Port of safeagents' `guard.py`.
//!
//! 1. Self-protection: calls against the guard itself are blocked outright.
//! 2. Undeclared tools and hosts from nowhere are blocked outright.
//! 3. (Not ported yet: atif-scan findings and the judge model.)

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Map, Value};

use crate::journal::Journal;
use crate::policy::{DEFAULT_ALLOWED_HOSTS, Policy};
use crate::selfprotect::SelfProtection;

pub type EventSink = Arc<dyn Fn(&CallEvent) + Send + Sync>;

pub struct Config {
    /// Replace flagged tool calls instead of only logging them.
    pub block: bool,
    /// `None` turns self-protection off.
    pub self_protection: Option<SelfProtection>,
    /// Hosts tool calls may reach even if the conversation never mentions them.
    pub allowed_hosts: Vec<String>,
    /// Keeps calls that weren't simply passed.
    pub journal: Option<Journal>,
    /// Told about every reviewed call (the app's activity view).
    pub on_event: Option<EventSink>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            block: true,
            self_protection: None,
            allowed_hosts: DEFAULT_ALLOWED_HOSTS.iter().map(|h| h.to_string()).collect(),
            journal: None,
            on_event: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Passed,
    /// Would have been blocked, but the guard is in log-only mode.
    Flagged,
    Blocked,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Review {
    pub reason: Option<String>,
}

impl Review {
    pub fn verdict(&self) -> Verdict {
        if self.reason.is_some() { Verdict::Blocked } else { Verdict::Passed }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Blocked {
    pub name: String,
    pub reason: String,
}

impl Blocked {
    pub fn notice(&self) -> String {
        format!("[guard] blocked call to `{}`: {}", self.name, self.reason)
    }
}

/// What the app shows for one reviewed call.
#[derive(Debug, Clone, Serialize)]
pub struct CallEvent {
    pub time: f64,
    pub verdict: Verdict,
    pub tool: String,
    pub summary: String,
    pub reason: Option<String>,
    pub model: String,
    pub journal_id: Option<u64>,
}

pub struct Guard {
    request: Value,
    config: Arc<Config>,
    policy: Policy,
    pub flagged: Vec<Blocked>,
}

const SUMMARY_KEYS: &[&str] = &["command", "cmd", "url", "query", "file_path", "filePath", "path", "pattern", "code"];
const MAX_SUMMARY: usize = 80;

/// The one argument that says what a call does, else the raw arguments, on one line.
pub fn summary(args: &str) -> String {
    let parsed = serde_json::from_str::<Value>(args).ok();
    let text = parsed
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|o| SUMMARY_KEYS.iter().find_map(|k| o.get(*k).and_then(Value::as_str).filter(|s| !s.is_empty())))
        .unwrap_or(args);
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() > MAX_SUMMARY {
        text.chars().take(MAX_SUMMARY - 1).chain(['~']).collect()
    } else {
        text
    }
}

/// The user's messages, for the journal.
fn task_text(request: &Value) -> String {
    let Some(messages) = request.get("messages").and_then(Value::as_array) else { return String::new() };
    messages
        .iter()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        .map(|m| content_text(m.get("content")))
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Text of a message `content`: a string, or the text parts of a list.
pub fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|p| matches!(p.get("type").and_then(Value::as_str), Some("text" | "input_text")))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

impl Guard {
    pub fn new(request: Value, config: Arc<Config>) -> Self {
        let policy = Policy::new(&request, &config.allowed_hosts);
        Self { request, config, policy, flagged: Vec::new() }
    }

    pub fn blocking(&self) -> bool {
        self.config.block
    }

    /// Reviews a complete call without recording it.
    pub fn review(&self, name: &str, args: &str) -> Review {
        // Self-protection runs first, so nothing that later excuses findings can lift it.
        if let Some(reason) = self.config.self_protection.as_ref().and_then(|p| p.reason(args)) {
            return Review { reason: Some(reason) };
        }
        Review { reason: self.policy.check(name, args) }
    }

    /// Reviews and records a complete call; true if it may run (always, in log-only mode).
    pub fn allow(&mut self, name: &str, args: &str) -> bool {
        let review = self.review(name, args);
        let mut verdict = review.verdict();
        if let Some(reason) = &review.reason {
            self.flagged.push(Blocked { name: name.to_string(), reason: reason.clone() });
            if !self.config.block {
                verdict = Verdict::Flagged;
            }
        }
        let time = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
        let model = self.request.get("model").and_then(Value::as_str).unwrap_or("").to_string();

        let mut journal_id = None;
        if verdict != Verdict::Passed
            && let Some(journal) = &self.config.journal
        {
            let mut entry = Map::new();
            entry.insert("time".into(), time.into());
            entry.insert("verdict".into(), serde_json::to_value(verdict).unwrap());
            entry.insert("tool".into(), name.into());
            entry.insert("arguments".into(), args.into());
            entry.insert("reason".into(), review.reason.clone().into());
            entry.insert("model".into(), model.clone().into());
            entry.insert("task".into(), task_text(&self.request).into());
            match journal.append(entry) {
                Ok(id) => journal_id = Some(id),
                Err(e) => tracing::warn!("guard journal: {e}"),
            }
        }
        if verdict != Verdict::Passed {
            tracing::warn!("guard {verdict:?} call to `{name}`: {}", review.reason.as_deref().unwrap_or(""));
        }
        if let Some(sink) = &self.config.on_event {
            sink(&CallEvent { time, verdict, tool: name.to_string(), summary: summary(args), reason: review.reason.clone(), model, journal_id });
        }
        review.reason.is_none() || !self.config.block
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::tests::bash_request;

    #[test]
    fn log_only_flags_but_allows() {
        let config = Arc::new(Config { block: false, ..Config::default() });
        let mut guard = Guard::new(bash_request("hi"), config);
        assert!(guard.allow("shell", "{}"));
        assert_eq!(guard.flagged.len(), 1);
    }

    #[test]
    fn self_protection_runs_before_policy() {
        let protection = SelfProtection::new(&["agent-superviser".into()], &["me.aagedal.AgentSuperviser".into()]);
        let config = Arc::new(Config { self_protection: Some(protection), ..Config::default() });
        let guard = Guard::new(bash_request("hi"), config);
        let review = guard.review("bash", r#"{"command": "pkill agent-superviser"}"#);
        assert!(review.reason.unwrap().starts_with("self-protection:"));
    }

    #[test]
    fn events_reach_the_sink() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink_seen = seen.clone();
        let config = Arc::new(Config {
            on_event: Some(Arc::new(move |e: &CallEvent| sink_seen.lock().unwrap().push((e.verdict, e.summary.clone())))),
            ..Config::default()
        });
        let mut guard = Guard::new(bash_request("hi"), config);
        assert!(guard.allow("bash", r#"{"command": "ls   -la"}"#));
        assert!(!guard.allow("nope", "{}"));
        assert_eq!(*seen.lock().unwrap(), [(Verdict::Passed, "ls -la".to_string()), (Verdict::Blocked, "{}".to_string())]);
    }
}
