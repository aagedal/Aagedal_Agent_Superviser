//! Reviews the tool calls of one response to one request. Port of safeagents' `guard.py`.
//!
//! 1. Self-protection: calls against the guard itself are blocked outright.
//! 2. Undeclared tools and hosts from nowhere are blocked outright, and so are tools and
//!    programs the request's project turned off (its allowed sites count as mentioned).
//! 3. Scanner findings (`superviser-scan`, a port of atif-scan's local-safety checks) at or
//!    above `block_severity` are blocked too; milder ones make a call suspicious: the judge,
//!    if there is one, decides. Without one, suspicious calls run and are logged.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Map, Value};
use superviser_scan::{Scanner, Severity, Signal};

use crate::journal::Journal;
use crate::judge::Judge;
use crate::policy::{DEFAULT_ALLOWED_HOSTS, Policy};
use crate::project::ProjectRules;
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
    /// `None` turns the scanner off.
    pub scanner: Option<Arc<Scanner>>,
    /// Scanner findings at or above this block the call.
    pub block_severity: Severity,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            block: true,
            self_protection: None,
            allowed_hosts: DEFAULT_ALLOWED_HOSTS.iter().map(|h| h.to_string()).collect(),
            journal: None,
            on_event: None,
            scanner: Some(Arc::new(Scanner::new())),
            block_severity: Severity::High,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Passed,
    /// Ran, but the scanner found something below the blocking severity.
    Suspicious,
    /// Would have been blocked, but the guard is in log-only mode.
    Flagged,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Review {
    pub reason: Option<String>,
    pub signals: Vec<Signal>,
    /// The judge's answer as `allow: why`, `block: why` or `error: why`.
    pub judge: Option<String>,
}

impl Review {
    fn blocked(reason: String) -> Self {
        Self { reason: Some(reason), ..Self::default() }
    }

    /// `Blocked` with a reason, `Suspicious` with only signals, else `Passed`.
    pub fn verdict(&self) -> Verdict {
        if self.reason.is_some() {
            Verdict::Blocked
        } else if !self.signals.is_empty() {
            Verdict::Suspicious
        } else {
            Verdict::Passed
        }
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
    /// Scanner findings, as `id (severity)`.
    pub signals: Vec<String>,
    /// The judge's answer, as in [`Review::judge`].
    pub judge: Option<String>,
    pub model: String,
    pub journal_id: Option<u64>,
    /// The project the request came from, if it came through a project's path.
    pub project: Option<String>,
}

pub struct Guard {
    request: Value,
    config: Arc<Config>,
    policy: Policy,
    project: Option<Arc<ProjectRules>>,
    judge: Option<Judge>,
    pub flagged: Vec<Blocked>,
    /// This response's calls so far: the scanner judges the last in their light.
    calls: Vec<(String, String)>,
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

/// User turns kept for the judge and the journal, in characters.
const MAX_TASK: usize = 6000;

/// The text of user turns, for the judge and the journal: no system prompt (long and
/// generic in most harnesses), tool results or agent turns. Keeps the end when long, as the
/// latest ask matters most.
pub(crate) fn task_text(request: &Value) -> String {
    let text = if let Some(messages) = request.get("messages").and_then(Value::as_array) {
        messages
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("user"))
            .map(|m| content_text(m.get("content")))
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    } else if let Some(input) = request.get("input") {
        match input {
            Value::String(s) => s.clone(),
            Value::Array(items) => items
                .iter()
                .filter(|m| m.get("role").and_then(Value::as_str) == Some("user"))
                .map(|m| content_text(m.get("content")))
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n"),
            _ => String::new(),
        }
    } else {
        return String::new();
    };
    let length = text.chars().count();
    if length <= MAX_TASK {
        return text;
    }
    format!("[...]\n{}", text.chars().skip(length - MAX_TASK).collect::<String>())
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
        Self { request, config, policy, project: None, judge: None, flagged: Vec::new(), calls: Vec::new() }
    }

    /// Applies `project`'s rules: its sites are allowed, its turned-off tools and programs blocked.
    pub fn with_project(mut self, project: Arc<ProjectRules>) -> Self {
        let hosts: Vec<String> = self.config.allowed_hosts.iter().chain(&project.allowed_hosts).cloned().collect();
        self.policy = Policy::new(&self.request, &hosts);
        self.project = Some(project);
        self
    }

    /// Asks `judge` about suspicious calls.
    pub fn with_judge(mut self, judge: Judge) -> Self {
        self.judge = Some(judge);
        self
    }

    pub fn blocking(&self) -> bool {
        self.config.block
    }

    pub(crate) fn request(&self) -> &Value {
        &self.request
    }

    /// Reviews a complete call without logging it.
    pub async fn review(&mut self, name: &str, args: &str) -> Review {
        self.calls.push((name.to_string(), args.to_string()));
        // Self-protection runs first, so nothing that later excuses findings can lift it.
        if let Some(reason) = self.config.self_protection.as_ref().and_then(|p| p.reason(args)) {
            return Review::blocked(reason);
        }
        if let Some(reason) = self.policy.check(name, args) {
            return Review::blocked(reason);
        }
        if let Some(reason) = self.project.as_ref().and_then(|p| p.check(name, args)) {
            return Review::blocked(reason);
        }
        let Some(scanner) = &self.config.scanner else { return Review::default() };
        let signals = scanner.signals(&self.request, &self.calls);
        let severe: Vec<String> = signals.iter().filter(|s| s.severity >= self.config.block_severity).map(Signal::to_string).collect();
        if !severe.is_empty() {
            return Review { reason: Some(format!("scan: {}", severe.join(", "))), signals, judge: None };
        }
        if signals.is_empty() {
            return Review::default();
        }
        self.judge(name, args, signals).await
    }

    async fn judge(&self, name: &str, args: &str, signals: Vec<Signal>) -> Review {
        let Some(judge) = &self.judge else { return Review { signals, ..Review::default() } };
        let findings = signals.iter().map(Signal::to_string).collect::<Vec<_>>().join(", ");
        match judge.review(&self.request, name, args, &signals).await {
            Ok(d) if d.block => Review { reason: Some(format!("{findings}; judge: {}", d.reason)), signals, judge: Some(format!("block: {}", d.reason)) },
            Ok(d) => Review { reason: None, signals, judge: Some(format!("allow: {}", d.reason)) },
            Err(e) => {
                let reason = judge.options.fail_closed.then(|| format!("{findings}; judge unavailable"));
                Review { reason, signals, judge: Some(format!("error: {e}")) }
            }
        }
    }

    /// Reviews and records a complete call; true if it may run (always, in log-only mode).
    pub async fn allow(&mut self, name: &str, args: &str) -> bool {
        let review = self.review(name, args).await;
        let mut verdict = review.verdict();
        if let Some(reason) = &review.reason {
            self.flagged.push(Blocked { name: name.to_string(), reason: reason.clone() });
            if self.config.block {
                // A blocked call never runs; keep it out of scanner trace context for later calls.
                self.calls.pop();
            } else {
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
            entry.insert("signals".into(), serde_json::to_value(&review.signals).unwrap());
            entry.insert("judge".into(), review.judge.clone().into());
            entry.insert("model".into(), model.clone().into());
            entry.insert("task".into(), task_text(&self.request).into());
            if let Some(project) = &self.project {
                entry.insert("project".into(), project.name.clone().into());
            }
            match journal.append(entry) {
                Ok(id) => journal_id = Some(id),
                Err(e) => tracing::warn!("guard journal: {e}"),
            }
        }
        let signals: Vec<String> = review.signals.iter().map(Signal::to_string).collect();
        if verdict != Verdict::Passed {
            let why = review.reason.clone().unwrap_or_else(|| signals.join(", "));
            let judge = review.judge.as_ref().map_or(String::new(), |j| format!(" (judge {j})"));
            tracing::warn!("guard {verdict:?} call to `{name}`: {why}{judge}");
        }
        if let Some(sink) = &self.config.on_event {
            let event = CallEvent {
                time,
                verdict,
                tool: name.to_string(),
                summary: summary(args),
                reason: review.reason.clone(),
                signals,
                judge: review.judge.clone(),
                model,
                journal_id,
                project: self.project.as_ref().map(|p| p.name.clone()),
            };
            sink(&event);
        }
        review.reason.is_none() || !self.config.block
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::judge::JudgeOptions;
    use crate::policy::tests::bash_request;

    #[tokio::test]
    async fn log_only_flags_but_allows() {
        let config = Arc::new(Config { block: false, ..Config::default() });
        let mut guard = Guard::new(bash_request("hi"), config);
        assert!(guard.allow("shell", "{}").await);
        assert_eq!(guard.flagged.len(), 1);
    }

    #[tokio::test]
    async fn self_protection_runs_before_policy() {
        let protection = SelfProtection::new(&["agent-superviser".into()], &["me.aagedal.AgentSuperviser".into()]);
        let config = Arc::new(Config { self_protection: Some(protection), ..Config::default() });
        let mut guard = Guard::new(bash_request("hi"), config);
        let review = guard.review("bash", r#"{"command": "pkill agent-superviser"}"#).await;
        assert!(review.reason.unwrap().starts_with("self-protection:"));
    }

    #[tokio::test]
    async fn events_reach_the_sink() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink_seen = seen.clone();
        let config = Arc::new(Config {
            on_event: Some(Arc::new(move |e: &CallEvent| sink_seen.lock().unwrap().push((e.verdict, e.summary.clone())))),
            ..Config::default()
        });
        let mut guard = Guard::new(bash_request("hi"), config);
        assert!(guard.allow("bash", r#"{"command": "ls   -la"}"#).await);
        assert!(!guard.allow("nope", "{}").await);
        assert_eq!(*seen.lock().unwrap(), [(Verdict::Passed, "ls -la".to_string()), (Verdict::Blocked, "{}".to_string())]);
    }

    #[tokio::test]
    async fn scanner_findings_block_or_warn_by_severity() {
        // The user names the address, so the policy lets the call through to the scanner.
        let mut guard = Guard::new(bash_request("GitHub is at 140.82.112.3 here"), Arc::new(Config::default()));
        // network.isolation_bypass is high: blocked.
        let pinned = guard.review("bash", r#"{"command": "curl --resolve github.com:443:140.82.112.3 https://github.com/x"}"#).await;
        assert_eq!(pinned.verdict(), Verdict::Blocked);
        assert!(pinned.reason.unwrap().contains("network.isolation_bypass"));
        // side_channel.agent_history_probe is medium: runs, but suspicious.
        let history = guard.review("bash", r#"{"command": "ls ~/.claude/projects/"}"#).await;
        assert_eq!(history.verdict(), Verdict::Suspicious);
        assert!(guard.allow("bash", r#"{"command": "cat ~/.codex/sessions/x.jsonl"}"#).await);
    }

    /// A decision model that answers every judge request with P(block) `p`, or fails.
    async fn judge_server(p: Option<f64>) -> Judge {
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        let app = axum::Router::new().fallback(move || async move {
            match p {
                Some(p) => axum::Json(serde_json::json!({"answers": {"block": {"type": "noul", "noul": p}}})).into_response(),
                None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Judge::new(reqwest::Client::new(), &format!("http://{addr}"), None, JudgeOptions::default())
    }

    const HISTORY: &str = r#"{"command": "ls ~/.claude/projects/"}"#;

    #[tokio::test]
    async fn judge_decides_suspicious_calls() {
        let config = Arc::new(Config::default());
        let mut blocked = Guard::new(bash_request("fix the CSS"), config.clone()).with_judge(judge_server(Some(0.97)).await);
        let review = blocked.review("bash", HISTORY).await;
        assert_eq!(review.verdict(), Verdict::Blocked);
        assert_eq!(review.reason.unwrap(), "side_channel.agent_history_probe (medium); judge: 97% likely the task doesn't need it");
        assert_eq!(review.judge.unwrap(), "block: 97% likely the task doesn't need it");

        let mut allowed = Guard::new(bash_request("fix the CSS"), config).with_judge(judge_server(Some(0.4)).await);
        let review = allowed.review("bash", HISTORY).await;
        assert_eq!(review.verdict(), Verdict::Suspicious);
        assert!(review.judge.unwrap().starts_with("allow: "));
    }

    #[tokio::test]
    async fn judge_is_not_asked_about_clean_or_blocked_calls() {
        // A judge that would block everything: clean calls still pass, blocked ones keep their reason.
        let mut guard = Guard::new(bash_request("hi"), Arc::new(Config::default())).with_judge(judge_server(Some(1.0)).await);
        assert_eq!(guard.review("bash", r#"{"command": "ls"}"#).await, Review::default());
        assert_eq!(guard.review("shell", "{}").await.judge, None);
    }

    #[tokio::test]
    async fn unreachable_judge_fails_open_unless_told_otherwise() {
        let judge = judge_server(None).await;
        let mut open = Guard::new(bash_request("hi"), Arc::new(Config::default())).with_judge(judge.clone());
        let review = open.review("bash", HISTORY).await;
        assert_eq!(review.verdict(), Verdict::Suspicious);
        assert!(review.judge.unwrap().starts_with("error: "));

        let mut closed = judge;
        closed.options.fail_closed = true;
        let mut closed = Guard::new(bash_request("hi"), Arc::new(Config::default())).with_judge(closed);
        let review = closed.review("bash", HISTORY).await;
        assert_eq!(review.reason.unwrap(), "side_channel.agent_history_probe (medium); judge unavailable");
    }

    #[test]
    fn long_tasks_keep_their_end() {
        let request = bash_request(&format!("{}END", "a".repeat(MAX_TASK)));
        let task = task_text(&request);
        assert!(task.starts_with("[...]\n") && task.ends_with("END"));
        assert_eq!(task.chars().count(), MAX_TASK + 6);
    }
}
