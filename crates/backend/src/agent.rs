//! The built-in agent's loop. It sends a chat to the model through the guard, on the
//! project's path so the project's rules apply, runs the tool calls that come back (asking
//! the user first when the project says to), and emits updates on [`EventBus`] as it goes.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use futures_util::StreamExt;
use serde_json::{Value, json};
use superviser_core::agent::tools::{self, Kind, Workspace};
use superviser_core::agent::{Chat, system_prompt};
use superviser_core::awake::Awake;
use superviser_core::project::{Approvals, Folder, Project};
use superviser_core::sandbox::{Isolation, seatbelt};
use superviser_core::secrets;
use tokio::sync::{oneshot, watch};

use crate::events::{AppEvent, ChatEvent, EventBus};
use crate::state::App;
use crate::subagents::{SubagentRecord, SubagentState, SubagentStep, now_ms, title_case_role};

/// Model replies in one turn before the agent stops and hands back to the user.
const MAX_STEPS: usize = 60;
/// Maximum steps an isolated read-only sub-agent may take before summarizing.
const MAX_SUBAGENT_STEPS: usize = 12;
/// How often the reply being written is sent to the window.
const DELTA_EVERY: Duration = Duration::from_millis(50);
const DECLINED: &str = "The user declined this call. Ask what they'd like instead, or find another way they'd accept.";
const NOT_RUN: &str = "Not run: the user stopped the agent.";

struct RunningTurn {
    project: String,
    started_at: Instant,
    stop: watch::Sender<bool>,
}

/// Turns in progress and calls waiting for the user.
#[derive(Default)]
pub struct Chats {
    /// By chat id: running turn metadata and stop signal.
    running: Mutex<HashMap<String, RunningTurn>>,
    /// Chats deleted while a turn was stopping, so the exiting turn never recreates their file.
    deleted: Mutex<HashSet<String>>,
    /// By `chat:call`.
    approvals: Mutex<HashMap<String, oneshot::Sender<bool>>>,
}

impl Chats {
    pub fn busy(&self, chat: &str) -> bool {
        self.running.lock().unwrap().contains_key(chat)
    }

    pub fn busy_since(&self, chat: &str) -> Option<Instant> {
        self.running.lock().unwrap().get(chat).map(|turn| turn.started_at)
    }

    pub fn stop(&self, chat: &str) {
        if let Some(turn) = self.running.lock().unwrap().get(chat) {
            let _ = turn.stop.send(true);
        }
    }

    /// Stops `chat` and marks it deleted so an active turn won't save its file back to disk.
    pub fn stop_and_delete(&self, chat: &str) {
        let running = self.running.lock().unwrap();
        if let Some(turn) = running.get(chat) {
            self.deleted.lock().unwrap().insert(chat.to_string());
            let _ = turn.stop.send(true);
        }
    }

    /// Stops every active turn for `project` and marks them deleted.
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

    /// Stops every active turn (when the app quits).
    pub fn stop_all(&self) {
        for turn in self.running.lock().unwrap().values() {
            let _ = turn.stop.send(true);
        }
    }

    fn is_deleted(&self, chat: &str) -> bool {
        self.deleted.lock().unwrap().contains(chat)
    }

    pub fn answer(&self, chat: &str, call: &str, allow: bool) {
        if let Some(answer) = self.approvals.lock().unwrap().remove(&format!("{chat}:{call}")) {
            let _ = answer.send(allow);
        }
    }
}

/// Adds the user's message to a chat (a new one without `chat`) and starts the agent on it.
/// Returns the chat's id at once; the rest arrives as [`AppEvent::Chat`] events on `app.bus`.
pub fn send(app: Arc<App>, project: &str, chat: Option<&str>, text: &str, model: &str) -> anyhow::Result<String> {
    let project = app.project(project).context("that project no longer exists")?;
    if project.isolation != Isolation::MacosSandbox {
        bail!(
            "The built-in agent runs projects in the macOS sandbox only; the chat runs Linux VM projects with OpenCode, which this build doesn't include. Open this one in OpenCode, or switch it to the macOS sandbox in Projects."
        );
    }
    let (model, _) = app.loaded(model).context("Load the chat's model first.")?;
    app.touch(&model.id);
    let mut chat = match chat {
        Some(id) => Chat::load(&app.paths, &project.id, id)?,
        None => Chat::new(&project.id),
    };
    let (stop, stopped) = watch::channel(false);
    {
        let mut running = app.chats.running.lock().unwrap();
        if running.contains_key(&chat.id) {
            bail!("The agent is still working on this chat.");
        }
        running.insert(
            chat.id.clone(),
            RunningTurn {
                project: project.id.clone(),
                started_at: Instant::now(),
                stop,
            },
        );
    }
    app.chats.deleted.lock().unwrap().remove(&chat.id);
    let message = json!({ "role": "user", "content": text });
    chat.messages.push(message.clone());
    if let Err(e) = chat.save(&app.paths) {
        app.chats.running.lock().unwrap().remove(&chat.id);
        return Err(e);
    }
    let id = chat.id.clone();
    app.bus.emit(AppEvent::Chat(ChatEvent::State { chat: id.clone(), busy: true, error: None }));
    app.bus.emit(AppEvent::Chat(ChatEvent::Message { chat: id.clone(), message }));
    let sync_app = app.clone();
    tokio::spawn(async move {
        sync_app.sync_upstreams().await;
    });
    tokio::spawn(async move {
        let _awake = Awake::hold();
        let result = turn(&app, &project, &model.alias, &mut chat, stopped).await;
        app.chats.running.lock().unwrap().remove(&chat.id);
        app.chats.deleted.lock().unwrap().remove(&chat.id);
        app.bus.emit(AppEvent::Chat(ChatEvent::State {
            chat: chat.id.clone(),
            busy: false,
            error: result.err().map(|e| format!("{e:#}")),
        }));
    });
    Ok(id)
}

/// The model's replies and the calls they make, until it answers without calls, the user
/// stops it, or it has taken [`MAX_STEPS`].
async fn turn(
    app: &App,
    project: &Project,
    model: &str,
    chat: &mut Chat,
    mut stopped: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let main = std::fs::canonicalize(project.main())
        .with_context(|| format!("{} is gone", project.main().display()))?;
    let related: Vec<Folder> = project
        .related()
        .iter()
        .filter_map(|f| Some(Folder { path: std::fs::canonicalize(&f.path).ok()?, writable: f.writable }))
        .collect();
    let (commands, edits) = (project.allows("commands"), project.allows("edits"));
    let profile = if commands {
        Some(seatbelt::write_project_profile(
            &app.paths,
            &main,
            &related,
            project.network,
            &project.blocked_programs,
            &format!("chat-{}", project.id),
        )?)
    } else {
        None
    };
    let folders: Vec<Folder> = std::iter::once(Folder { path: main.clone(), writable: true })
        .chain(related.iter().cloned())
        .collect();
    let workspace = Workspace::new(&folders, profile, edits)?;
    let mut tools = workspace.definitions();
    if project.allows("subagents") {
        tools.push(subagent_tool_definition());
    }
    let system = json!({ "role": "system", "content": system_prompt(project, &main, &related, commands, edits) });
    let guard = app.guard_url().context("the guard is not running")?;
    let url = format!("{}{}/v1/chat/completions", guard.trim_end_matches("/v1"), project.guard_path());
    let key = secrets::local_api_key(&app.paths)?;
    let client = reqwest::Client::new();

    for _ in 0..MAX_STEPS {
        if *stopped.borrow() {
            return Ok(());
        }
        let messages: Vec<Value> = std::iter::once(system.clone())
            .chain(chat.messages.iter().map(for_model))
            .collect();
        let body = json!({ "model": model, "messages": messages, "tools": tools, "stream": true });
        let reply = tokio::select! {
            reply = stream_reply(&app.bus, &client, &url, &key, &body, &chat.id) => reply?,
            _ = stopped.wait_for(|s| *s) => return Ok(()),
        };
        let mut message = json!({ "role": "assistant", "content": reply.content });
        if !reply.reasoning.is_empty() {
            message["reasoning"] = json!(reply.reasoning);
        }
        if !reply.calls.is_empty() {
            message["tool_calls"] = json!(
                reply
                    .calls
                    .iter()
                    .map(|c| json!({ "id": c.id, "type": "function", "function": { "name": c.name, "arguments": c.arguments } }))
                    .collect::<Vec<_>>()
            );
        }
        add(app, chat, message)?;
        if reply.calls.is_empty() {
            return Ok(());
        }
        for call in &reply.calls {
            let result = if *stopped.borrow() {
                NOT_RUN.to_string()
            } else if call.name == "subagent" && project.allows("subagents") {
                run_subagent(app, project, &chat.id, model, &folders, call, stopped.clone()).await
            } else if !asks(project.approvals, &call.name) || approved(app, &chat.id, call, &mut stopped).await {
                workspace.run(&call.name, &call.arguments, stopped.clone()).await
            } else if *stopped.borrow() {
                NOT_RUN.to_string()
            } else {
                DECLINED.to_string()
            };
            add(app, chat, json!({ "role": "tool", "tool_call_id": call.id, "content": result }))?;
        }
    }
    bail!("The agent took {MAX_STEPS} steps without finishing; send a message to let it continue.")
}

fn subagent_tool_definition() -> Value {
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

/// Picks the best currently-loaded model for a sub-agent: if the machine has another lighter
/// model loaded alongside `parent_alias`, route the sub-agent there to avoid evicting the
/// main model's KV cache; otherwise stay on `parent_alias`.
fn pick_subagent_model(app: &App, parent_alias: &str) -> (String, String) {
    let loaded = app.loaded_models();
    if let Some((other, _)) = loaded
        .iter()
        .filter(|(m, _)| m.alias != parent_alias)
        .min_by_key(|(m, _)| m.model.quant.size)
    {
        return (other.alias.clone(), other.model.name.clone());
    }
    let name = loaded
        .iter()
        .find(|(m, _)| m.alias == parent_alias)
        .map(|(m, _)| m.model.name.clone())
        .unwrap_or_else(|| parent_alias.to_string());
    (parent_alias.to_string(), name)
}

/// Runs an isolated read-only sub-agent turn, streaming its live status, tool steps, and
/// final summary through `app.subagents`.
async fn run_subagent(
    app: &App,
    project: &Project,
    parent_chat_id: &str,
    parent_model_alias: &str,
    folders: &[Folder],
    call: &Call,
    mut parent_stopped: watch::Receiver<bool>,
) -> String {
    let args: Value = serde_json::from_str(if call.arguments.trim().is_empty() {
        "{}"
    } else {
        &call.arguments
    })
    .unwrap_or_else(|_| json!({}));
    let role = title_case_role(args["role"].as_str().unwrap_or("Explore"));
    let title = args["description"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("Investigate codebase")
        .trim()
        .to_string();
    let prompt = args["prompt"]
        .as_str()
        .or_else(|| args["task"].as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if prompt.is_empty() {
        return "Error: subagent requires a non-empty `prompt`.".into();
    }

    let (model_alias, model_name) = pick_subagent_model(app, parent_model_alias);
    let sub_id = call.id.clone();
    let (cancel_tx, mut cancel_rx) = watch::channel(false);
    app.subagents.register_cancel(&sub_id, cancel_tx);

    app.subagents.upsert(
        SubagentRecord {
            id: sub_id.clone(),
            project: project.id.clone(),
            parent_session: parent_chat_id.to_string(),
            child_session: None,
            role: role.clone(),
            title: title.clone(),
            prompt: prompt.clone(),
            model: Some(model_name),
            state: SubagentState::Running,
            started_ms: now_ms(),
            finished_ms: None,
            steps: Vec::new(),
            live_text: None,
            result: None,
            error: None,
            cancellable: true,
        },
        &app.bus,
    );

    let outcome: anyhow::Result<(SubagentState, String)> = async {
        // Read-only workspace (no edits, no shell) keeps sub-agents safe and conflict-free.
        let workspace = Workspace::new(folders, None, false)?;
        let tools = workspace.definitions();
        let guard = app.guard_url().context("the guard is not running")?;
        let url = format!(
            "{}{}/v1/chat/completions",
            guard.trim_end_matches("/v1"),
            project.guard_path()
        );
        let key = secrets::local_api_key(&app.paths)?;
        let client = reqwest::Client::new();
        let system = json!({
            "role": "system",
            "content": format!(
                "You are a focused read-only sub-agent ({role}) helping a parent coding agent on project \"{}\". \
                 Use `read`, `glob`, and `grep` to gather exact facts, file paths, and line numbers, then return a concise, structured summary directly answering the task.",
                project.name
            )
        });
        let mut messages = vec![system, json!({ "role": "user", "content": prompt })];

        for _ in 0..MAX_SUBAGENT_STEPS {
            if *parent_stopped.borrow() || *cancel_rx.borrow() {
                return Ok((SubagentState::Cancelled, NOT_RUN.to_string()));
            }
            let body = json!({
                "model": model_alias,
                "messages": messages,
                "tools": tools,
                "stream": true
            });
            let reply = tokio::select! {
                r = stream_subagent_reply(app, &sub_id, &client, &url, &key, &body) => r?,
                _ = parent_stopped.wait_for(|s| *s) => return Ok((SubagentState::Cancelled, NOT_RUN.to_string())),
                _ = cancel_rx.wait_for(|s| *s) => return Ok((SubagentState::Cancelled, "Stopped by user.".to_string())),
            };
            let mut msg = json!({ "role": "assistant", "content": reply.content });
            if !reply.calls.is_empty() {
                msg["tool_calls"] = json!(
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
            messages.push(msg);
            if reply.calls.is_empty() {
                return Ok((SubagentState::Completed, reply.content.trim().to_string()));
            }
            for sub_call in &reply.calls {
                if *parent_stopped.borrow() || *cancel_rx.borrow() {
                    return Ok((SubagentState::Cancelled, "Stopped by user.".to_string()));
                }
                let (verb, subject) = summarize_builtin_call(sub_call);
                app.subagents.update_with(&sub_id, &app.bus, |rec| {
                    rec.steps.push(SubagentStep {
                        id: sub_call.id.clone(),
                        verb,
                        subject,
                        status: "in_progress".into(),
                        output: None,
                    });
                });
                let tool_out = workspace
                    .run(&sub_call.name, &sub_call.arguments, cancel_rx.clone())
                    .await;
                let failed = tool_out.starts_with("Error:");
                app.subagents.update_with(&sub_id, &app.bus, |rec| {
                    if let Some(step) = rec.steps.iter_mut().find(|s| s.id == sub_call.id) {
                        step.status = if failed { "failed" } else { "completed" }.into();
                        let preview: String = tool_out.chars().take(600).collect();
                        step.output = Some(preview);
                    }
                });
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": sub_call.id,
                    "content": tool_out
                }));
            }
        }
        Ok((
            SubagentState::Completed,
            "Sub-agent reached its step limit without a final summary.".to_string(),
        ))
    }
    .await;

    app.subagents.unregister_cancel(&sub_id);

    match outcome {
        Ok((SubagentState::Completed, text)) => {
            app.subagents.update_with(&sub_id, &app.bus, |rec| {
                rec.state = SubagentState::Completed;
                rec.finished_ms = Some(now_ms());
                rec.cancellable = false;
                rec.live_text = None;
                rec.result = Some(text.clone());
            });
            text
        }
        Ok((SubagentState::Cancelled, msg)) => {
            app.subagents.update_with(&sub_id, &app.bus, |rec| {
                rec.state = SubagentState::Cancelled;
                rec.finished_ms = Some(now_ms());
                rec.cancellable = false;
                rec.error = Some(msg.clone());
            });
            msg
        }
        Ok((_, text)) => text,
        Err(e) => {
            let err = format!("Sub-agent error: {e:#}");
            app.subagents.update_with(&sub_id, &app.bus, |rec| {
                rec.state = SubagentState::Failed;
                rec.finished_ms = Some(now_ms());
                rec.cancellable = false;
                rec.error = Some(err.clone());
            });
            err
        }
    }
}

fn summarize_builtin_call(call: &Call) -> (String, String) {
    let args: Value = serde_json::from_str(&call.arguments).unwrap_or_default();
    let verb = match call.name.as_str() {
        "read" => "Read",
        "glob" | "grep" => "Search",
        other => other,
    }
    .to_string();
    let subject = args["path"]
        .as_str()
        .or_else(|| args["pattern"].as_str())
        .unwrap_or("")
        .to_string();
    (verb, subject)
}

async fn stream_subagent_reply(
    app: &App,
    sub_id: &str,
    client: &reqwest::Client,
    url: &str,
    key: &str,
    body: &Value,
) -> anyhow::Result<Reply> {
    let response = client
        .post(url)
        .bearer_auth(key)
        .json(body)
        .send()
        .await
        .context("couldn't reach the guard")?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        bail!("the model server said {status}: {text}");
    }
    let mut reply = Reply::default();
    let mut calls: Vec<Call> = Vec::new();
    let mut buffer: Vec<u8> = Vec::new();
    let mut last = Instant::now();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        buffer.extend_from_slice(&chunk.context("the sub-agent reply was cut off")?);
        while let Some((start, end)) = find_sse_event_end(&buffer) {
            let event: Vec<u8> = buffer.drain(..end).collect();
            consume_sse_event(&event[..start], &mut reply, &mut calls)?;
        }
        if last.elapsed() >= Duration::from_millis(120) && !reply.content.is_empty() {
            app.subagents.update_with(sub_id, &app.bus, |rec| {
                rec.live_text = Some(reply.content.clone());
            });
            last = Instant::now();
        }
    }
    if !buffer.is_empty() {
        consume_sse_event(&buffer, &mut reply, &mut calls)?;
    }
    for (i, call) in calls.iter_mut().enumerate() {
        if call.id.is_empty() {
            call.id = format!("subcall_{i}");
        }
    }
    reply.calls = calls.into_iter().filter(|c| !c.name.is_empty()).collect();
    Ok(reply)
}

/// A saved message as the model gets it: without our own fields.
fn for_model(message: &Value) -> Value {
    let mut message = message.clone();
    if let Some(object) = message.as_object_mut() {
        object.remove("reasoning");
    }
    message
}

fn add(app: &App, chat: &mut Chat, message: Value) -> anyhow::Result<()> {
    if app.chats.is_deleted(&chat.id) {
        return Ok(());
    }
    app.bus.emit(AppEvent::Chat(ChatEvent::Message {
        chat: chat.id.clone(),
        message: message.clone(),
    }));
    chat.messages.push(message);
    chat.save(&app.paths)
}

/// Whether the project wants the user asked before `tool` runs.
fn asks(approvals: Approvals, tool: &str) -> bool {
    match approvals {
        Approvals::Never => false,
        Approvals::Commands => tools::kind(tool) == Kind::Runs,
        Approvals::Everything => tools::kind(tool) != Kind::Reads,
    }
}

/// Asks the user about `call`; false if they decline or stop the agent.
async fn approved(app: &App, chat: &str, call: &Call, stopped: &mut watch::Receiver<bool>) -> bool {
    let (answer, answered) = oneshot::channel();
    let key = format!("{chat}:{}", call.id);
    app.chats.approvals.lock().unwrap().insert(key.clone(), answer);
    app.bus.emit(AppEvent::Chat(ChatEvent::Approval {
        chat: chat.to_string(),
        call: call.id.clone(),
        name: call.name.clone(),
        arguments: call.arguments.clone(),
    }));
    let allowed = tokio::select! {
        answer = answered => answer.unwrap_or(false),
        _ = stopped.wait_for(|s| *s) => false,
    };
    app.chats.approvals.lock().unwrap().remove(&key);
    allowed
}

#[derive(Debug, Default, Clone)]
struct Call {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Debug, Default)]
struct Reply {
    content: String,
    reasoning: String,
    calls: Vec<Call>,
}

/// Sends one request and reads the streamed reply, passing it on to the window as it comes.
async fn stream_reply(
    bus: &EventBus,
    client: &reqwest::Client,
    url: &str,
    key: &str,
    body: &Value,
    chat: &str,
) -> anyhow::Result<Reply> {
    let response = client
        .post(url)
        .bearer_auth(key)
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
    let mut reply = Reply::default();
    let mut calls: Vec<Call> = Vec::new();
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
            bus.emit(AppEvent::Chat(ChatEvent::Delta {
                chat: chat.to_string(),
                content: reply.content.clone(),
                reasoning: reply.reasoning.clone(),
            }));
            last = Instant::now();
        }
    }
    if !buffer.is_empty() {
        consume_sse_event(&buffer, &mut reply, &mut calls)?;
    }
    // Calls the model named but never gave an id still need one to be answered.
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
fn find_sse_event_end(buf: &[u8]) -> Option<(usize, usize)> {
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

fn consume_sse_event(raw: &[u8], reply: &mut Reply, calls: &mut Vec<Call>) -> anyhow::Result<()> {
    let event = String::from_utf8_lossy(raw);
    for data in event.lines().filter_map(|l| l.strip_prefix("data:")).map(str::trim) {
        if data == "[DONE]" {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else { continue };
        if let Some(error) = value.get("error") {
            bail!("the model server said: {}", error["message"].as_str().unwrap_or("an error"));
        }
        read_delta(&value["choices"][0]["delta"], reply, calls);
    }
    Ok(())
}

/// Adds one streamed delta to the reply: text, reasoning, and pieces of tool calls by index.
fn read_delta(delta: &Value, reply: &mut Reply, calls: &mut Vec<Call>) {
    if let Some(text) = delta["content"].as_str() {
        reply.content.push_str(text);
    }
    if let Some(text) = delta["reasoning_content"].as_str() {
        reply.reasoning.push_str(text);
    }
    for part in delta["tool_calls"].as_array().into_iter().flatten() {
        let index = part["index"].as_u64().unwrap_or(calls.len() as u64) as usize;
        if calls.len() <= index {
            calls.resize(index + 1, Call::default());
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
    fn assembles_streamed_tool_calls() {
        let mut reply = Reply::default();
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
    }

    #[test]
    fn asks_as_the_project_says() {
        assert!(asks(Approvals::Commands, "bash") && !asks(Approvals::Commands, "edit"));
        assert!(asks(Approvals::Everything, "write") && !asks(Approvals::Everything, "grep"));
        assert!(!asks(Approvals::Never, "bash"));
    }

    #[test]
    fn reasoning_stays_out_of_requests() {
        let saved = json!({"role": "assistant", "content": "x", "reasoning": "y"});
        assert_eq!(for_model(&saved), json!({"role": "assistant", "content": "x"}));
    }

    #[test]
    fn sse_byte_buffer_preserves_split_utf8_and_trailing_event() {
        let mut reply = Reply::default();
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
