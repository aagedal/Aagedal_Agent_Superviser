//! The harness turn engine: streaming OpenAI-compatible loop, mid-turn steering, approvals,
//! automatic context pruning & compaction, and pluggable sub-agent execution.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};

use crate::chat::{Chat, ChatStore};
use crate::context::{self, ContextBudget};
use crate::tools::{Kind, Workspace, kind};

/// Model replies in one turn before the agent pauses and hands back to the user.
pub const MAX_STEPS: usize = 60;
/// How often streamed reply deltas are flushed to the UI.
const DELTA_EVERY: Duration = Duration::from_millis(50);

const DECLINED: &str =
    "The user declined this call. Ask what they'd like instead, or find another way they'd accept.";
const NOT_RUN: &str = "Not run: the user stopped the agent.";

/// When the harness asks the user before running a tool call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ApprovalPolicy {
    /// Ask before shell commands; file edits run directly.
    #[default]
    Commands,
    /// Ask before shell commands and file edits.
    Everything,
    /// Never ask interactively (guard and sandbox still enforce rules).
    Never,
}

impl ApprovalPolicy {
    pub fn asks(self, tool: &str) -> bool {
        match self {
            Self::Never => false,
            Self::Commands => kind(tool) == Kind::Runs,
            Self::Everything => kind(tool) != Kind::Reads,
        }
    }
}

/// Events emitted by the harness during a turn.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HarnessEvent {
    /// A message was appended to the chat.
    Message { chat: String, message: Value },
    /// Live streamed assistant text and reasoning so far.
    Delta { chat: String, content: String, reasoning: String },
    /// A tool call waits for user approval.
    Approval { chat: String, call: String, name: String, arguments: String },
    /// Current context usage for the chat (real server tokens when available, else estimate).
    Usage { chat: String, used: u64, size: u64 },
    /// Informational status notice (e.g. automatic compaction or mid-turn steering applied).
    Notice { chat: String, text: String },
    /// Turn started or ended.
    State { chat: String, busy: bool, error: Option<String> },
}

/// Receives [`HarnessEvent`]s from the engine.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: HarnessEvent);
}

/// A parsed tool call from the model.
#[derive(Debug, Default, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Pluggable sub-agent executor (allows the host app to route read-only research to a
/// secondary lighter model and stream progress into its own UI tracker).
pub trait SubagentHandler: Send + Sync {
    fn tool_definition(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "subagent",
                "description": "Delegate a focused read-only investigation or analysis task (e.g. searching files, tracing how a feature works, reviewing code) to an isolated sub-agent with its own fresh context window. Returns a concise summary so your main context stays compact.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "role": { "type": "string", "description": "Short role name such as Explore, Research, or Review" },
                        "description": { "type": "string", "description": "Short 3-8 word label for the subtask" },
                        "prompt": { "type": "string", "description": "Clear, self-contained instructions for what the sub-agent should inspect and report back" }
                    },
                    "required": ["description", "prompt"]
                }
            }
        })
    }

    fn run(
        &self,
        chat_id: String,
        call: ToolCall,
        cancel: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = String> + Send + '_>>;
}

/// Endpoint and credentials for one OpenAI-compatible chat-completions model call.
#[derive(Debug, Clone)]
pub struct ModelEndpoint {
    /// Full `/v1/chat/completions` URL.
    pub completions_url: String,
    pub api_key: String,
    pub model: String,
    pub context_window: u64,
}

struct RunningTurn {
    project: String,
    started_at: Instant,
    stop: watch::Sender<bool>,
    steer_tx: mpsc::UnboundedSender<Value>,
}

/// Manages active turns, cancellation, Pi-style mid-turn steering, and tool approvals.
#[derive(Default)]
pub struct SessionManager {
    running: Mutex<HashMap<String, RunningTurn>>,
    deleted: Mutex<HashSet<String>>,
    approvals: Mutex<HashMap<String, oneshot::Sender<bool>>>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn busy(&self, chat: &str) -> bool {
        self.running.lock().unwrap().contains_key(chat)
    }

    pub fn busy_since(&self, chat: &str) -> Option<Instant> {
        self.running.lock().unwrap().get(chat).map(|turn| turn.started_at)
    }

    /// Registers a new running turn and returns `(stop_rx, steer_rx)`.
    pub fn begin_turn(
        &self,
        chat_id: &str,
        project_id: &str,
    ) -> anyhow::Result<(watch::Receiver<bool>, mpsc::UnboundedReceiver<Value>)> {
        let (stop, stop_rx) = watch::channel(false);
        let (steer_tx, steer_rx) = mpsc::unbounded_channel();
        {
            let mut running = self.running.lock().unwrap();
            if running.contains_key(chat_id) {
                bail!("The agent is still working on this chat.");
            }
            running.insert(
                chat_id.to_string(),
                RunningTurn {
                    project: project_id.to_string(),
                    started_at: Instant::now(),
                    stop,
                    steer_tx,
                },
            );
        }
        self.deleted.lock().unwrap().remove(chat_id);
        Ok((stop_rx, steer_rx))
    }

    pub fn end_turn(&self, chat_id: &str) {
        self.running.lock().unwrap().remove(chat_id);
        self.deleted.lock().unwrap().remove(chat_id);
    }

    /// Queues a steering user message into an already-running turn (Pi-style steering).
    /// Returns `true` if the chat was running and accepted the steering message.
    pub fn steer(&self, chat_id: &str, message: Value) -> bool {
        let running = self.running.lock().unwrap();
        if let Some(turn) = running.get(chat_id) {
            turn.steer_tx.send(message).is_ok()
        } else {
            false
        }
    }

    pub fn stop(&self, chat: &str) {
        if let Some(turn) = self.running.lock().unwrap().get(chat) {
            let _ = turn.stop.send(true);
        }
    }

    pub fn stop_and_delete(&self, chat: &str) {
        let running = self.running.lock().unwrap();
        if let Some(turn) = running.get(chat) {
            self.deleted.lock().unwrap().insert(chat.to_string());
            let _ = turn.stop.send(true);
        }
    }

    pub fn stop_project(&self, project: &str) {
        let running = self.running.lock().unwrap();
        let mut deleted = self.deleted.lock().unwrap();
        for (chat, turn) in running.iter() {
            if turn.project == project {
                deleted.insert(chat.clone());
                let _ = turn.stop.send(true);
            }
        }
    }

    pub fn stop_all(&self) {
        for turn in self.running.lock().unwrap().values() {
            let _ = turn.stop.send(true);
        }
    }

    pub fn is_deleted(&self, chat: &str) -> bool {
        self.deleted.lock().unwrap().contains(chat)
    }

    pub fn answer(&self, chat: &str, call: &str, allow: bool) {
        if let Some(answer) = self.approvals.lock().unwrap().remove(&format!("{chat}:{call}")) {
            let _ = answer.send(allow);
        }
    }

    async fn wait_approval(
        &self,
        sink: &dyn EventSink,
        chat: &str,
        call: &ToolCall,
        stopped: &mut watch::Receiver<bool>,
    ) -> bool {
        let (answer, answered) = oneshot::channel();
        let key = format!("{chat}:{}", call.id);
        self.approvals.lock().unwrap().insert(key.clone(), answer);
        sink.emit(HarnessEvent::Approval {
            chat: chat.to_string(),
            call: call.id.clone(),
            name: call.name.clone(),
            arguments: call.arguments.clone(),
        });
        let allowed = tokio::select! {
            answer = answered => answer.unwrap_or(false),
            _ = stopped.wait_for(|s| *s) => false,
        };
        self.approvals.lock().unwrap().remove(&key);
        allowed
    }
}

/// Configuration for running one agent turn.
pub struct TurnConfig {
    pub store: ChatStore,
    pub endpoint: ModelEndpoint,
    pub system_prompt: String,
    pub workspace: Workspace,
    pub approvals: ApprovalPolicy,
    pub subagents: Option<Arc<dyn SubagentHandler>>,
    pub max_steps: usize,
}

/// Runs one full agent turn (streaming replies, running tools, draining mid-turn steering,
/// pruning stale tool outputs, and auto-compacting when approaching the context limit).
pub async fn run_turn(
    sessions: &SessionManager,
    sink: &dyn EventSink,
    cfg: TurnConfig,
    chat: &mut Chat,
    mut stopped: watch::Receiver<bool>,
    mut steer_rx: mpsc::UnboundedReceiver<Value>,
) -> anyhow::Result<()> {
    let mut tools = cfg.workspace.definitions();
    if let Some(sub) = &cfg.subagents {
        tools.push(sub.tool_definition());
    }
    let system = json!({ "role": "system", "content": cfg.system_prompt });
    let client = reqwest::Client::new();
    let budget = ContextBudget::for_window(cfg.endpoint.context_window);
    let mut last_reported_prompt_tokens: Option<u64> = None;

    for _ in 0..cfg.max_steps {
        if *stopped.borrow() {
            return Ok(());
        }

        // Drain any Pi-style mid-turn steering messages queued while tools were running.
        drain_steering(sessions, sink, &cfg.store, chat, &mut steer_rx)?;

        // Check context usage and auto-compact if crossing the compaction threshold.
        let mut active = context::active_messages(chat);
        let est_before = context::estimate_request_tokens(&system, &active, &tools);
        let effective_used = last_reported_prompt_tokens.map_or(est_before, |r| r.max(est_before));

        if effective_used >= budget.compact_threshold()
            && let Some(split) = context::compaction_split_index(chat, budget.keep_recent_messages)
            && let Ok(summary) = run_compaction_summary(&client, &cfg.endpoint, chat, split).await
        {
            chat.set_compaction(split, summary);
            if !sessions.is_deleted(&chat.id) {
                cfg.store.save(chat)?;
            }
            active = context::active_messages(chat);
            let est_after = context::estimate_request_tokens(&system, &active, &tools);
            sink.emit(HarnessEvent::Notice {
                chat: chat.id.clone(),
                text: format!(
                    "Compacted earlier turns to keep context lean (~{}k → ~{}k tokens).",
                    effective_used.div_ceil(1_000),
                    est_after.div_ceil(1_000)
                ),
            });
        }

        // Stage 1 soft prune of older large tool outputs if still above prune threshold.
        let pruned = context::prune_stale_tool_outputs(&system, &mut active, &tools, &budget);
        let est_now = context::estimate_request_tokens(&system, &active, &tools);
        sink.emit(HarnessEvent::Usage {
            chat: chat.id.clone(),
            used: est_now,
            size: budget.context_window,
        });
        if pruned > 0 {
            tracing::debug!(chat = %chat.id, pruned, est_now, "pruned stale tool outputs for context budget");
        }

        let mut messages = Vec::with_capacity(1 + active.len());
        messages.push(system.clone());
        messages.extend(active);

        let body = json!({
            "model": cfg.endpoint.model,
            "messages": messages,
            "tools": tools,
            "stream": true,
            "stream_options": { "include_usage": true }
        });

        let reply = tokio::select! {
            reply = stream_reply(sink, &client, &cfg.endpoint, &body, &chat.id) => reply?,
            _ = stopped.wait_for(|s| *s) => return Ok(()),
        };

        if let Some(usage) = reply.usage {
            last_reported_prompt_tokens = Some(usage.total_tokens);
            sink.emit(HarnessEvent::Usage {
                chat: chat.id.clone(),
                used: usage.total_tokens,
                size: budget.context_window,
            });
        } else {
            last_reported_prompt_tokens = None;
        }

        let mut message = json!({ "role": "assistant", "content": reply.content });
        if !reply.reasoning.is_empty() {
            message["reasoning"] = json!(reply.reasoning);
        }
        if !reply.calls.is_empty() {
            message["tool_calls"] = json!(
                reply
                    .calls
                    .iter()
                    .map(|c| json!({
                        "id": c.id,
                        "type": "function",
                        "function": { "name": c.name, "arguments": c.arguments }
                    }))
                    .collect::<Vec<_>>()
            );
        }
        append_and_save(sessions, sink, &cfg.store, chat, message)?;

        if reply.calls.is_empty() {
            // If a steering message arrived right as the model finished answering, continue the turn!
            if drain_steering(sessions, sink, &cfg.store, chat, &mut steer_rx)? > 0 {
                continue;
            }
            return Ok(());
        }

        for call in &reply.calls {
            let result = if *stopped.borrow() {
                NOT_RUN.to_string()
            } else if call.name == "subagent"
                && let Some(sub) = &cfg.subagents
            {
                sub.run(chat.id.clone(), call.clone(), stopped.clone()).await
            } else if !cfg.approvals.asks(&call.name)
                || sessions.wait_approval(sink, &chat.id, call, &mut stopped).await
            {
                cfg.workspace.run(&call.name, &call.arguments, stopped.clone()).await
            } else if *stopped.borrow() {
                NOT_RUN.to_string()
            } else {
                DECLINED.to_string()
            };
            append_and_save(
                sessions,
                sink,
                &cfg.store,
                chat,
                json!({ "role": "tool", "tool_call_id": call.id, "content": result }),
            )?;
        }
    }

    bail!("The agent took {} steps without finishing; send a message to let it continue.", cfg.max_steps)
}

/// Runs an explicit manual compaction (e.g. from `/compact`) on an idle chat.
pub async fn compact_chat(
    store: &ChatStore,
    endpoint: &ModelEndpoint,
    chat: &mut Chat,
    keep_recent_messages: usize,
) -> anyhow::Result<Option<String>> {
    let Some(split) = context::compaction_split_index(chat, keep_recent_messages) else {
        return Ok(None);
    };
    let client = reqwest::Client::new();
    let summary = run_compaction_summary(&client, endpoint, chat, split).await?;
    chat.set_compaction(split, summary.clone());
    store.save(chat)?;
    Ok(Some(summary))
}

async fn run_compaction_summary(
    client: &reqwest::Client,
    endpoint: &ModelEndpoint,
    chat: &Chat,
    split_index: usize,
) -> anyhow::Result<String> {
    let body = json!({
        "model": endpoint.model,
        "messages": context::build_compaction_prompt(chat, split_index),
        "stream": false,
        "max_tokens": 1024
    });
    let response = client
        .post(&endpoint.completions_url)
        .bearer_auth(&endpoint.api_key)
        .json(&body)
        .send()
        .await
        .context("couldn't reach the guard for compaction")?;
    if !response.status().is_success() {
        bail!("compaction request failed with {}", response.status());
    }
    let val: Value = response.json().await.context("invalid compaction response")?;
    let summary = val["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();
    anyhow::ensure!(!summary.is_empty(), "empty compaction summary");
    Ok(summary)
}

fn drain_steering(
    sessions: &SessionManager,
    sink: &dyn EventSink,
    store: &ChatStore,
    chat: &mut Chat,
    steer_rx: &mut mpsc::UnboundedReceiver<Value>,
) -> anyhow::Result<usize> {
    let mut count = 0;
    while let Ok(msg) = steer_rx.try_recv() {
        append_and_save(sessions, sink, store, chat, msg)?;
        count += 1;
    }
    Ok(count)
}

fn append_and_save(
    sessions: &SessionManager,
    sink: &dyn EventSink,
    store: &ChatStore,
    chat: &mut Chat,
    message: Value,
) -> anyhow::Result<()> {
    if sessions.is_deleted(&chat.id) {
        return Ok(());
    }
    sink.emit(HarnessEvent::Message {
        chat: chat.id.clone(),
        message: message.clone(),
    });
    chat.messages.push(message);
    store.save(chat)
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Default)]
pub struct StreamReply {
    pub content: String,
    pub reasoning: String,
    pub calls: Vec<ToolCall>,
    pub usage: Option<TokenUsage>,
}

async fn stream_reply(
    sink: &dyn EventSink,
    client: &reqwest::Client,
    endpoint: &ModelEndpoint,
    body: &Value,
    chat: &str,
) -> anyhow::Result<StreamReply> {
    let response = client
        .post(&endpoint.completions_url)
        .bearer_auth(&endpoint.api_key)
        .json(body)
        .send()
        .await
        .context("couldn't reach the guard")?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        let message = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
            .unwrap_or(text);
        bail!("the model server said {status}: {message}");
    }

    let mut reply = StreamReply::default();
    let mut calls: Vec<ToolCall> = Vec::new();
    let mut buffer: Vec<u8> = Vec::new();
    let mut last = Instant::now();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        buffer.extend_from_slice(&chunk.context("the reply was cut off")?);
        while let Some((start, end)) = find_sse_event_end(&buffer) {
            let event: Vec<u8> = buffer.drain(..end).collect();
            consume_sse_event(&event[..start], &mut reply, &mut calls)?;
        }
        if last.elapsed() >= DELTA_EVERY {
            sink.emit(HarnessEvent::Delta {
                chat: chat.to_string(),
                content: reply.content.clone(),
                reasoning: reply.reasoning.clone(),
            });
            last = Instant::now();
        }
    }
    if !buffer.is_empty() {
        consume_sse_event(&buffer, &mut reply, &mut calls)?;
    }
    for (i, call) in calls.iter_mut().enumerate() {
        if call.id.is_empty() {
            call.id = format!("call_{i}");
        }
    }
    reply.calls = calls.into_iter().filter(|c| !c.name.is_empty()).collect();
    Ok(reply)
}

/// Finds the first `\n\n` or `\r\n\r\n` boundary in a raw byte buffer so multi-byte UTF-8
/// characters split across HTTP chunks are never decoded mid-sequence.
pub fn find_sse_event_end(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some((i, i + 2));
        }
        if i + 3 < buf.len() && &buf[i..i + 4] == b"\r\n\r\n" {
            return Some((i, i + 4));
        }
        i += 1;
    }
    None
}

pub fn consume_sse_event(
    raw: &[u8],
    reply: &mut StreamReply,
    calls: &mut Vec<ToolCall>,
) -> anyhow::Result<()> {
    let event = String::from_utf8_lossy(raw);
    for data in event.lines().filter_map(|l| l.strip_prefix("data:")).map(str::trim) {
        if data == "[DONE]" {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else { continue };
        if let Some(error) = value.get("error") {
            bail!(
                "the model server said: {}",
                error["message"].as_str().unwrap_or("an error")
            );
        }
        if let Some(usage) = value.get("usage").and_then(Value::as_object) {
            let prompt_tokens = usage.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
            let completion_tokens = usage.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0);
            let total_tokens = usage
                .get("total_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(prompt_tokens + completion_tokens);
            if total_tokens > 0 {
                reply.usage = Some(TokenUsage {
                    prompt_tokens,
                    completion_tokens,
                    total_tokens,
                });
            }
        }
        read_delta(&value["choices"][0]["delta"], reply, calls);
    }
    Ok(())
}

pub fn read_delta(delta: &Value, reply: &mut StreamReply, calls: &mut Vec<ToolCall>) {
    if let Some(text) = delta["content"].as_str() {
        reply.content.push_str(text);
    }
    if let Some(text) = delta["reasoning_content"].as_str() {
        reply.reasoning.push_str(text);
    }
    for part in delta["tool_calls"].as_array().into_iter().flatten() {
        let index = part["index"].as_u64().unwrap_or(calls.len() as u64) as usize;
        if calls.len() <= index {
            calls.resize(index + 1, ToolCall::default());
        }
        let call = &mut calls[index];
        if let Some(id) = part["id"].as_str() {
            call.id = id.to_string();
        }
        if let Some(name) = part["function"]["name"].as_str() {
            call.name.push_str(name);
        }
        if let Some(arguments) = part["function"]["arguments"].as_str() {
            call.arguments.push_str(arguments);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assembles_streamed_tool_calls_and_usage() {
        let mut reply = StreamReply::default();
        let mut calls = Vec::new();
        for delta in [
            json!({"reasoning_content": "Look first."}),
            json!({"tool_calls": [{"index": 0, "id": "a", "function": {"name": "read", "arguments": "{\"pa"}}]}),
            json!({"tool_calls": [{"index": 0, "function": {"arguments": "th\": \"x\"}"}}]}),
            json!({"tool_calls": [{"index": 1, "id": "b", "function": {"name": "bash", "arguments": "{}"}}]}),
            json!({"content": "Done."}),
        ] {
            read_delta(&delta, &mut reply, &mut calls);
        }
        assert_eq!((reply.content.as_str(), reply.reasoning.as_str()), ("Done.", "Look first."));
        assert_eq!((calls[0].name.as_str(), calls[0].arguments.as_str()), ("read", "{\"path\": \"x\"}"));
        assert_eq!(calls[1].id, "b");

        let usage_event = b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1200,\"completion_tokens\":300,\"total_tokens\":1500}}";
        consume_sse_event(usage_event, &mut reply, &mut calls).unwrap();
        assert_eq!(reply.usage.unwrap().total_tokens, 1500);
    }

    #[test]
    fn steering_delivers_message_into_running_turn() {
        let mgr = SessionManager::new();
        assert!(!mgr.steer("c1", json!({"role": "user", "content": "nope"})));
        let (_stop, mut steer_rx) = mgr.begin_turn("c1", "p1").unwrap();
        assert!(mgr.steer("c1", json!({"role": "user", "content": "focus on tests"})));
        let msg = steer_rx.try_recv().unwrap();
        assert_eq!(msg["content"], "focus on tests");
        mgr.end_turn("c1");
    }

    #[test]
    fn approval_policy_matches_tool_kinds() {
        assert!(ApprovalPolicy::Commands.asks("bash") && !ApprovalPolicy::Commands.asks("edit"));
        assert!(ApprovalPolicy::Everything.asks("write") && !ApprovalPolicy::Everything.asks("grep"));
        assert!(!ApprovalPolicy::Never.asks("bash"));
    }

    #[test]
    fn sse_byte_buffer_preserves_split_utf8() {
        let mut reply = StreamReply::default();
        let mut calls = Vec::new();
        let stream = "data: {\"choices\":[{\"delta\":{\"content\":\"café — done\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"!\"}}]}";
        let mut buffer = Vec::new();
        for chunk in stream.as_bytes().chunks(3) {
            buffer.extend_from_slice(chunk);
            while let Some((start, end)) = find_sse_event_end(&buffer) {
                let event: Vec<u8> = buffer.drain(..end).collect();
                consume_sse_event(&event[..start], &mut reply, &mut calls).unwrap();
            }
        }
        if !buffer.is_empty() {
            consume_sse_event(&buffer, &mut reply, &mut calls).unwrap();
        }
        assert_eq!(reply.content, "café — done!");
    }
}
