//! The built-in agent's loop. It sends a chat to the model through the guard, on the
//! project's path so the project's rules apply, runs the tool calls that come back (asking
//! the user first when the project says to), and tells the chat window as it goes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{Value, json};
use superviser_core::agent::tools::{self, Kind, Workspace};
use superviser_core::agent::{Chat, system_prompt};
use superviser_core::project::{Approvals, Folder, Project};
use superviser_core::sandbox::{Isolation, seatbelt};
use superviser_core::secrets;
use tauri::{AppHandle, Emitter};
use tokio::sync::{oneshot, watch};

use crate::state::App;

/// Model replies in one turn before the agent stops and hands back to the user.
const MAX_STEPS: usize = 60;
/// How often the reply being written is sent to the window.
const DELTA_EVERY: Duration = Duration::from_millis(50);
const DECLINED: &str = "The user declined this call. Ask what they'd like instead, or find another way they'd accept.";
const NOT_RUN: &str = "Not run: the user stopped the agent.";

/// Turns in progress and calls waiting for the user.
#[derive(Default)]
pub struct Chats {
    /// By chat id; sending `true` stops the turn.
    running: Mutex<HashMap<String, watch::Sender<bool>>>,
    /// By `chat:call`.
    approvals: Mutex<HashMap<String, oneshot::Sender<bool>>>,
}

impl Chats {
    pub fn busy(&self, chat: &str) -> bool {
        self.running.lock().unwrap().contains_key(chat)
    }

    pub fn stop(&self, chat: &str) {
        if let Some(stop) = self.running.lock().unwrap().get(chat) {
            let _ = stop.send(true);
        }
    }

    pub fn answer(&self, chat: &str, call: &str, allow: bool) {
        if let Some(answer) = self.approvals.lock().unwrap().remove(&format!("{chat}:{call}")) {
            let _ = answer.send(allow);
        }
    }
}

/// What the chat window hears about, on the `chat` event.
#[derive(Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Event<'a> {
    /// A message was added to the chat.
    Message { chat: &'a str, message: &'a Value },
    /// The reply so far.
    Delta { chat: &'a str, content: &'a str, reasoning: &'a str },
    /// A call waits for the user's yes or no.
    Approval { chat: &'a str, call: &'a str, name: &'a str, arguments: &'a str },
    /// The turn started or ended (with what went wrong, if something did).
    State { chat: &'a str, busy: bool, error: Option<String> },
}

fn emit(handle: &AppHandle, event: Event) {
    let _ = handle.emit("chat", event);
}

/// Adds the user's message to a chat (a new one without `chat`) and starts the agent on it.
/// Returns the chat's id at once; the rest arrives as events.
pub fn send(handle: AppHandle, app: Arc<App>, project: &str, chat: Option<&str>, text: &str) -> anyhow::Result<String> {
    let project = app.project(project).context("that project no longer exists")?;
    if project.isolation != Isolation::MacosSandbox {
        bail!("The built-in chat runs projects in the macOS sandbox so far. Open this one in OpenCode, or switch it to the macOS sandbox in Projects.");
    }
    let (model, _) = app.running_model().context("Start a model first.")?;
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
        running.insert(chat.id.clone(), stop);
    }
    let message = json!({ "role": "user", "content": text });
    chat.messages.push(message.clone());
    if let Err(e) = chat.save(&app.paths) {
        app.chats.running.lock().unwrap().remove(&chat.id);
        return Err(e);
    }
    let id = chat.id.clone();
    emit(&handle, Event::State { chat: &id, busy: true, error: None });
    emit(&handle, Event::Message { chat: &id, message: &message });
    tauri::async_runtime::spawn(async move {
        let result = turn(&handle, &app, &project, &model.alias, &mut chat, stopped).await;
        app.chats.running.lock().unwrap().remove(&chat.id);
        emit(&handle, Event::State { chat: &chat.id, busy: false, error: result.err().map(|e| format!("{e:#}")) });
    });
    Ok(id)
}

/// The model's replies and the calls they make, until it answers without calls, the user
/// stops it, or it has taken [`MAX_STEPS`].
async fn turn(handle: &AppHandle, app: &App, project: &Project, model: &str, chat: &mut Chat, mut stopped: watch::Receiver<bool>) -> anyhow::Result<()> {
    let main = std::fs::canonicalize(project.main()).with_context(|| format!("{} is gone", project.main().display()))?;
    let related: Vec<Folder> = project.related().iter().filter_map(|f| Some(Folder { path: std::fs::canonicalize(&f.path).ok()?, writable: f.writable })).collect();
    let (commands, edits) = (project.allows("commands"), project.allows("edits"));
    let profile = if commands {
        Some(seatbelt::write_project_profile(&app.paths, &main, &related, project.network, &project.blocked_programs, &format!("chat-{}", project.id))?)
    } else {
        None
    };
    let folders: Vec<Folder> = std::iter::once(Folder { path: main.clone(), writable: true }).chain(related.iter().cloned()).collect();
    let workspace = Workspace::new(&folders, profile, edits)?;
    let tools = workspace.definitions();
    let system = json!({ "role": "system", "content": system_prompt(project, &main, &related, commands, edits) });
    let guard = app.guard_url().context("the guard is not running")?;
    let url = format!("{}{}/v1/chat/completions", guard.trim_end_matches("/v1"), project.guard_path());
    let key = secrets::local_api_key(&app.paths)?;
    let client = reqwest::Client::new();

    for _ in 0..MAX_STEPS {
        if *stopped.borrow() {
            return Ok(());
        }
        let messages: Vec<Value> = std::iter::once(system.clone()).chain(chat.messages.iter().map(for_model)).collect();
        let body = json!({ "model": model, "messages": messages, "tools": tools, "stream": true });
        let reply = tokio::select! {
            reply = stream_reply(handle, &client, &url, &key, &body, &chat.id) => reply?,
            _ = stopped.wait_for(|s| *s) => return Ok(()),
        };
        let mut message = json!({ "role": "assistant", "content": reply.content });
        if !reply.reasoning.is_empty() {
            message["reasoning"] = json!(reply.reasoning);
        }
        if !reply.calls.is_empty() {
            message["tool_calls"] = json!(reply.calls.iter().map(|c| json!({ "id": c.id, "type": "function", "function": { "name": c.name, "arguments": c.arguments } })).collect::<Vec<_>>());
        }
        add(handle, app, chat, message)?;
        if reply.calls.is_empty() {
            return Ok(());
        }
        for call in &reply.calls {
            let result = if *stopped.borrow() {
                NOT_RUN.to_string()
            } else if !asks(project.approvals, &call.name) || approved(handle, app, &chat.id, call, &mut stopped).await {
                workspace.run(&call.name, &call.arguments, stopped.clone()).await
            } else if *stopped.borrow() {
                NOT_RUN.to_string()
            } else {
                DECLINED.to_string()
            };
            add(handle, app, chat, json!({ "role": "tool", "tool_call_id": call.id, "content": result }))?;
        }
    }
    bail!("The agent took {MAX_STEPS} steps without finishing; send a message to let it continue.")
}

/// A saved message as the model gets it: without our own fields.
fn for_model(message: &Value) -> Value {
    let mut message = message.clone();
    if let Some(object) = message.as_object_mut() {
        object.remove("reasoning");
    }
    message
}

fn add(handle: &AppHandle, app: &App, chat: &mut Chat, message: Value) -> anyhow::Result<()> {
    emit(handle, Event::Message { chat: &chat.id, message: &message });
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
async fn approved(handle: &AppHandle, app: &App, chat: &str, call: &Call, stopped: &mut watch::Receiver<bool>) -> bool {
    let (answer, answered) = oneshot::channel();
    let key = format!("{chat}:{}", call.id);
    app.chats.approvals.lock().unwrap().insert(key.clone(), answer);
    emit(handle, Event::Approval { chat, call: &call.id, name: &call.name, arguments: &call.arguments });
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
async fn stream_reply(handle: &AppHandle, client: &reqwest::Client, url: &str, key: &str, body: &Value, chat: &str) -> anyhow::Result<Reply> {
    let response = client.post(url).bearer_auth(key).json(body).send().await.context("couldn't reach the guard")?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        let message = serde_json::from_str::<Value>(&text).ok().and_then(|v| v["error"]["message"].as_str().map(str::to_string)).unwrap_or(text);
        bail!("the model server said {status}: {message}");
    }
    let mut reply = Reply::default();
    let mut calls: Vec<Call> = Vec::new();
    let mut buffer = String::new();
    let mut last = Instant::now();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        buffer.push_str(&String::from_utf8_lossy(&chunk.context("the reply was cut off")?));
        while let Some(end) = buffer.find("\n\n") {
            let event: String = buffer.drain(..end + 2).collect();
            for data in event.lines().filter_map(|l| l.strip_prefix("data:")).map(str::trim) {
                if data == "[DONE]" {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(data) else { continue };
                if let Some(error) = value.get("error") {
                    bail!("the model server said: {}", error["message"].as_str().unwrap_or("an error"));
                }
                read_delta(&value["choices"][0]["delta"], &mut reply, &mut calls);
            }
        }
        if last.elapsed() >= DELTA_EVERY {
            emit(handle, Event::Delta { chat, content: &reply.content, reasoning: &reply.reasoning });
            last = Instant::now();
        }
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
}
