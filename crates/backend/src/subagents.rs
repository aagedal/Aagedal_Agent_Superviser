//! First-class app-managed sub-agent tracking.
//!
//! Tracks sub-agents triggered during a chat — both OpenCode ACP `task`/`subagent` tool calls
//! and the built-in agent's isolated `subagent` runs — so the UI can show a live T3 Code-style
//! sub-agent strip, elapsed timers, step timelines, model badges, cancellation, and an
//! inspector drawer without cluttering the main conversation.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::watch;

use crate::events::{AppEvent, EventBus};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentState {
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl SubagentState {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Running)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "stopped",
        }
    }
}

/// One tool call or notable action taken inside a sub-agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentStep {
    pub id: String,
    pub verb: String,
    pub subject: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

/// Live snapshot of one sub-agent run tied to a parent chat session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentRecord {
    pub id: String,
    pub project: String,
    pub parent_session: String,
    /// OpenCode child session id when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_session: Option<String>,
    pub role: String,
    pub title: String,
    pub prompt: String,
    /// Model name running this sub-agent (helps highlight when a secondary fast model handles it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub state: SubagentState,
    pub started_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_ms: Option<u64>,
    pub steps: Vec<SubagentStep>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// True when the host app directly owns cancellation for this sub-agent.
    pub cancellable: bool,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Registry of active and recent sub-agents across all projects and sessions.
#[derive(Default)]
pub struct SubagentTracker {
    /// Ordered list of ids (creation order) + records by id.
    order: Mutex<Vec<String>>,
    records: Mutex<HashMap<String, SubagentRecord>>,
    /// Direct cancellation channels for built-in/app-managed sub-agents.
    cancels: Mutex<HashMap<String, watch::Sender<bool>>>,
}

impl SubagentTracker {
    pub fn upsert(&self, record: SubagentRecord, bus: &EventBus) {
        let id = record.id.clone();
        {
            let mut records = self.records.lock().unwrap();
            if !records.contains_key(&id) {
                self.order.lock().unwrap().push(id.clone());
            }
            records.insert(id, record.clone());
        }
        bus.emit(AppEvent::Subagent(record));
    }

    pub fn update_with(
        &self,
        id: &str,
        bus: &EventBus,
        mutate: impl FnOnce(&mut SubagentRecord),
    ) -> Option<SubagentRecord> {
        let updated = {
            let mut records = self.records.lock().unwrap();
            let rec = records.get_mut(id)?;
            mutate(rec);
            rec.clone()
        };
        bus.emit(AppEvent::Subagent(updated.clone()));
        Some(updated)
    }

    pub fn register_cancel(&self, id: &str, tx: watch::Sender<bool>) {
        self.cancels.lock().unwrap().insert(id.to_string(), tx);
    }

    pub fn unregister_cancel(&self, id: &str) {
        self.cancels.lock().unwrap().remove(id);
    }

    /// Cancels an app-managed sub-agent if it has a registered stop channel.
    pub fn cancel(&self, id: &str, bus: &EventBus) -> bool {
        let tx = self.cancels.lock().unwrap().remove(id);
        let Some(tx) = tx else {
            return false;
        };
        let _ = tx.send(true);
        self.update_with(id, bus, |rec| {
            if rec.state == SubagentState::Running {
                rec.state = SubagentState::Cancelled;
                rec.finished_ms = Some(now_ms());
                rec.cancellable = false;
            }
        });
        true
    }

    /// Cancels all running app-managed sub-agents belonging to `parent_session`.
    pub fn cancel_for_session(&self, parent_session: &str, bus: &EventBus) {
        let ids: Vec<String> = self
            .records
            .lock()
            .unwrap()
            .values()
            .filter(|r| r.parent_session == parent_session && r.state == SubagentState::Running)
            .map(|r| r.id.clone())
            .collect();
        for id in ids {
            self.cancel(&id, bus);
        }
    }

    /// Sub-agents triggered in `(project, parent_session)`, in creation order.
    pub fn for_session(&self, project: &str, parent_session: &str) -> Vec<SubagentRecord> {
        let order = self.order.lock().unwrap();
        let records = self.records.lock().unwrap();
        order
            .iter()
            .filter_map(|id| records.get(id))
            .filter(|r| r.project == project && r.parent_session == parent_session)
            .cloned()
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<SubagentRecord> {
        self.records.lock().unwrap().get(id).cloned()
    }

    /// Finds a tracked sub-agent whose `child_session` matches `session`.
    pub fn find_by_child_session(&self, project: &str, child_session: &str) -> Option<String> {
        self.records
            .lock()
            .unwrap()
            .values()
            .find(|r| r.project == project && r.child_session.as_deref() == Some(child_session))
            .map(|r| r.id.clone())
    }

    /// Inspects an OpenCode ACP `session/update` payload and updates tracked sub-agents when:
    /// 1. The update is a `tool_call` / `tool_call_update` invoking `task` or `subagent`, or
    /// 2. The update belongs to a known `child_session` of a tracked sub-agent.
    pub fn ingest_acp_update(
        &self,
        project: &str,
        session: &str,
        update: &Value,
        default_model: Option<String>,
        bus: &EventBus,
    ) {
        // Case 1: update arriving directly for a known child session.
        if let Some(parent_subagent_id) = self.find_by_child_session(project, session) {
            self.ingest_child_session_update(&parent_subagent_id, update, bus);
            return;
        }

        // Case 2: `task` / `subagent` tool call inside a parent session.
        let kind = update["sessionUpdate"].as_str().unwrap_or("");
        if !matches!(kind, "tool_call" | "tool_call_update") {
            return;
        }
        let Some(call_id) = update["toolCallId"].as_str().filter(|s| !s.is_empty()) else {
            return;
        };

        let already_tracked = self.records.lock().unwrap().contains_key(call_id);
        if !already_tracked && !looks_like_subagent_call(update) {
            return;
        }

        let existing = self.get(call_id);
        let mut rec = existing.unwrap_or_else(|| SubagentRecord {
            id: call_id.to_string(),
            project: project.to_string(),
            parent_session: session.to_string(),
            child_session: None,
            role: "Sub-agent".into(),
            title: "Delegated task".into(),
            prompt: String::new(),
            model: default_model.clone(),
            state: SubagentState::Running,
            started_ms: now_ms(),
            finished_ms: None,
            steps: Vec::new(),
            live_text: None,
            result: None,
            error: None,
            cancellable: false,
        });

        let raw_in = &update["rawInput"];
        if let Some(role) = raw_in["subagent_type"]
            .as_str()
            .or_else(|| raw_in["subagentType"].as_str())
            .or_else(|| raw_in["role"].as_str())
            .filter(|s| !s.trim().is_empty())
        {
            rec.role = title_case_role(role);
        }
        if let Some(desc) = raw_in["description"]
            .as_str()
            .or_else(|| raw_in["title"].as_str())
            .filter(|s| !s.trim().is_empty())
        {
            rec.title = desc.trim().to_string();
        } else if rec.title == "Delegated task"
            && let Some(t) = update["title"].as_str()
            && !t.trim().is_empty()
            && !t.eq_ignore_ascii_case("task")
            && !t.eq_ignore_ascii_case("subagent")
        {
            rec.title = t.trim().to_string();
        }
        if let Some(prompt) = raw_in["prompt"]
            .as_str()
            .or_else(|| raw_in["task"].as_str())
            .filter(|s| !s.trim().is_empty())
        {
            rec.prompt = prompt.trim().to_string();
        }
        if rec.model.is_none() {
            rec.model = default_model;
        }

        // Extract child session id and nested step summaries when OpenCode includes them.
        if let Some(child_sid) = extract_child_session_id(update) {
            rec.child_session = Some(child_sid);
        }
        let extracted_steps = extract_summary_steps(update);
        if !extracted_steps.is_empty() {
            rec.steps = extracted_steps;
        }

        // Extract textual content / output.
        let text = extract_tool_content_text(update);
        if !text.is_empty() {
            if rec.child_session.is_none()
                && let Some(sid) = parse_task_id_from_text(&text)
            {
                rec.child_session = Some(sid);
            }
            if matches!(update["status"].as_str(), Some("failed")) {
                rec.error = Some(text);
            } else if matches!(update["status"].as_str(), Some("completed")) {
                rec.result = Some(text);
                rec.live_text = None;
            } else {
                rec.live_text = Some(text);
            }
        }

        match update["status"].as_str() {
            Some("completed") => {
                rec.state = SubagentState::Completed;
                rec.finished_ms.get_or_insert_with(now_ms);
            }
            Some("failed") => {
                rec.state = SubagentState::Failed;
                rec.finished_ms.get_or_insert_with(now_ms);
            }
            Some("cancelled") => {
                rec.state = SubagentState::Cancelled;
                rec.finished_ms.get_or_insert_with(now_ms);
            }
            Some("in_progress") | Some("pending") => {
                rec.state = SubagentState::Running;
            }
            _ => {}
        }

        self.upsert(rec, bus);
    }

    fn ingest_child_session_update(&self, id: &str, update: &Value, bus: &EventBus) {
        let kind = update["sessionUpdate"].as_str().unwrap_or("");
        match kind {
            "agent_message_chunk" => {
                let chunk = update["content"]["text"].as_str().unwrap_or("");
                if chunk.is_empty() {
                    return;
                }
                self.update_with(id, bus, |rec| {
                    let buf = rec.live_text.get_or_insert_with(String::new);
                    buf.push_str(chunk);
                });
            }
            "tool_call" | "tool_call_update" => {
                let Some(step_id) = update["toolCallId"].as_str().filter(|s| !s.is_empty()) else {
                    return;
                };
                let verb = step_verb(update);
                let subject = step_subject(update);
                let status = update["status"].as_str().unwrap_or("in_progress").to_string();
                let output = {
                    let t = extract_tool_content_text(update);
                    (!t.is_empty()).then_some(t)
                };
                self.update_with(id, bus, |rec| {
                    if let Some(existing) = rec.steps.iter_mut().find(|s| s.id == step_id) {
                        if !verb.is_empty() {
                            existing.verb = verb;
                        }
                        if !subject.is_empty() {
                            existing.subject = subject;
                        }
                        existing.status = status;
                        if output.is_some() {
                            existing.output = output;
                        }
                    } else {
                        rec.steps.push(SubagentStep {
                            id: step_id.to_string(),
                            verb,
                            subject,
                            status,
                            output,
                        });
                    }
                });
            }
            _ => {}
        }
    }
}

/// Returns true if an ACP `tool_call` / `tool_call_update` payload represents a sub-agent invocation.
pub fn looks_like_subagent_call(update: &Value) -> bool {
    let name = update["name"].as_str().unwrap_or("");
    let title = update["title"].as_str().unwrap_or("");
    if name.eq_ignore_ascii_case("task")
        || name.eq_ignore_ascii_case("subagent")
        || title.eq_ignore_ascii_case("task")
        || title.eq_ignore_ascii_case("subagent")
    {
        return true;
    }
    let raw_in = &update["rawInput"];
    raw_in.get("subagent_type").and_then(Value::as_str).is_some()
        || raw_in.get("subagentType").and_then(Value::as_str).is_some()
        || (raw_in.get("prompt").and_then(Value::as_str).is_some()
            && raw_in.get("description").and_then(Value::as_str).is_some()
            && update["kind"].as_str().unwrap_or("") != "execute")
}

pub fn title_case_role(raw: &str) -> String {
    let trimmed = raw.trim().replace(['_', '-'], " ");
    let mut out = String::new();
    for (i, word) in trimmed.split_whitespace().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    if out.is_empty() {
        "Sub-agent".into()
    } else {
        out
    }
}

fn extract_child_session_id(update: &Value) -> Option<String> {
    let candidates = [
        update.pointer("/rawOutput/sessionId"),
        update.pointer("/rawOutput/session_id"),
        update.pointer("/rawOutput/metadata/sessionId"),
        update.pointer("/rawOutput/metadata/session_id"),
        update.pointer("/meta/sessionId"),
        update.pointer("/_meta/sessionId"),
        update.pointer("/rawInput/task_id"),
        update.pointer("/rawInput/sessionId"),
    ];
    candidates
        .into_iter()
        .flatten()
        .find_map(|v| v.as_str().filter(|s| !s.trim().is_empty()).map(str::to_string))
}

fn parse_task_id_from_text(text: &str) -> Option<String> {
    for line in text.lines().take(6) {
        let trimmed = line.trim();
        for prefix in ["task_id:", "session_id:", "sessionId:"] {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                let id = rest.split_whitespace().next().unwrap_or("").trim();
                if !id.is_empty() {
                    return Some(id.to_string());
                }
            }
        }
    }
    None
}

fn extract_summary_steps(update: &Value) -> Vec<SubagentStep> {
    let arr = update
        .pointer("/rawOutput/metadata/summary")
        .or_else(|| update.pointer("/rawOutput/summary"))
        .and_then(Value::as_array);
    let Some(items) = arr else {
        return Vec::new();
    };
    items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| {
            let tool = item["tool"].as_str().or_else(|| item["name"].as_str())?;
            let id = item["id"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| format!("step-{i}"));
            let status = item
                .pointer("/state/status")
                .or_else(|| item.get("status"))
                .and_then(Value::as_str)
                .unwrap_or("completed")
                .to_string();
            let subject = item
                .pointer("/state/title")
                .or_else(|| item.get("title"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Some(SubagentStep {
                id,
                verb: title_case_role(tool),
                subject,
                status,
                output: None,
            })
        })
        .collect()
}

fn extract_tool_content_text(update: &Value) -> String {
    let mut parts = Vec::new();
    for item in update["content"].as_array().into_iter().flatten() {
        if item["type"].as_str() == Some("content")
            && item["content"]["type"].as_str() == Some("text")
            && let Some(t) = item["content"]["text"].as_str()
            && !t.trim().is_empty()
        {
            parts.push(t.trim().to_string());
        }
    }
    if parts.is_empty()
        && let Some(s) = update["rawOutput"].as_str()
        && !s.trim().is_empty()
    {
        parts.push(s.trim().to_string());
    }
    parts.join("\n\n")
}

fn step_verb(update: &Value) -> String {
    match update["kind"].as_str().unwrap_or("") {
        "read" => "Read".into(),
        "edit" => "Edit".into(),
        "delete" => "Delete".into(),
        "move" => "Move".into(),
        "search" => "Search".into(),
        "execute" => "Run".into(),
        "fetch" => "Fetch".into(),
        _ => update["name"]
            .as_str()
            .or_else(|| update["title"].as_str())
            .map(title_case_role)
            .unwrap_or_else(|| "Tool".into()),
    }
}

fn step_subject(update: &Value) -> String {
    let input = &update["rawInput"];
    input["path"]
        .as_str()
        .or_else(|| input["filePath"].as_str())
        .or_else(|| input["pattern"].as_str())
        .or_else(|| input["query"].as_str())
        .or_else(|| input["command"].as_str())
        .or_else(|| update["title"].as_str())
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ingests_acp_task_lifecycle_and_child_session_updates() {
        let tracker = SubagentTracker::default();
        let bus = EventBus::new();
        let mut rx = bus.subscribe();

        let start = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_task_1",
            "name": "task",
            "title": "task",
            "status": "in_progress",
            "rawInput": {
                "subagent_type": "explore",
                "description": "Find auth middleware",
                "prompt": "Search the repo for JWT verification."
            }
        });
        tracker.ingest_acp_update("proj1", "sess_parent", &start, Some("Qwen3.8 27B".into()), &bus);

        let list = tracker.for_session("proj1", "sess_parent");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].role, "Explore");
        assert_eq!(list[0].title, "Find auth middleware");
        assert_eq!(list[0].state, SubagentState::Running);
        assert_eq!(list[0].model.as_deref(), Some("Qwen3.8 27B"));

        let complete = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call_task_1",
            "status": "completed",
            "rawOutput": {
                "metadata": {
                    "sessionId": "sess_child_1",
                    "summary": [
                        { "id": "s1", "tool": "grep", "state": { "status": "completed", "title": "verify_jwt" } },
                        { "id": "s2", "tool": "read", "state": { "status": "completed", "title": "src/auth.rs" } }
                    ]
                }
            },
            "content": [
                { "type": "content", "content": { "type": "text", "text": "JWT checks live in src/auth.rs." } }
            ]
        });
        tracker.ingest_acp_update("proj1", "sess_parent", &complete, None, &bus);

        let updated = tracker.get("call_task_1").unwrap();
        assert_eq!(updated.state, SubagentState::Completed);
        assert_eq!(updated.child_session.as_deref(), Some("sess_child_1"));
        assert_eq!(updated.steps.len(), 2);
        assert_eq!(updated.steps[0].verb, "Grep");
        assert_eq!(updated.result.as_deref(), Some("JWT checks live in src/auth.rs."));
        assert!(matches!(rx.try_recv(), Ok(AppEvent::Subagent(_))));
    }

    #[test]
    fn ignores_non_subagent_tool_calls() {
        let tracker = SubagentTracker::default();
        let bus = EventBus::new();
        let read = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_read_1",
            "kind": "read",
            "name": "read",
            "rawInput": { "path": "Cargo.toml" }
        });
        tracker.ingest_acp_update("proj1", "sess_parent", &read, None, &bus);
        assert!(tracker.for_session("proj1", "sess_parent").is_empty());
    }
}
