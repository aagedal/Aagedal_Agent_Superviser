//! Bridges the standalone [`entropy`] coding harness crate into Agent Superviser's runtime (`App`),
//! macOS Seatbelt sandbox, Apple `container` Linux VM, guard proxy, and live sub-agent tracker.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use futures_util::StreamExt;
use serde_json::{Value, json};
use superviser_core::awake::Awake;
use superviser_core::project::{Approvals, Folder, Project};
use superviser_core::sandbox::container::{self, ShellVm};
use superviser_core::sandbox::{Isolation, Network, seatbelt};
use superviser_core::secrets;
use entropy::{
    ApprovalPolicy, Chat, ChatStore, CommandOutput, CommandRunner, EventSink, HarnessEvent,
    MAX_STEPS, ModelEndpoint, PathMapper, PromptConfig, SessionManager, StreamReply,
    SubagentHandler, ToolCall, TurnConfig, Workspace, WorkspaceFolder, compact_chat, run_process,
    run_turn,
};
use tokio::sync::watch;

use crate::events::{AppEvent, ChatEvent, EventBus};
use crate::state::App;
use crate::subagents::{SubagentRecord, SubagentState, SubagentStep, now_ms, title_case_role};

/// Maximum steps an isolated read-only sub-agent may take before summarizing.
const MAX_SUBAGENT_STEPS: usize = 12;
const NOT_RUN: &str = "Not run: the user stopped the agent.";

/// Active harness turns and tool-approval channels.
#[derive(Default)]
pub struct Chats {
    sessions: SessionManager,
}

impl Chats {
    pub fn busy(&self, chat: &str) -> bool {
        self.sessions.busy(chat)
    }

    pub fn busy_since(&self, chat: &str) -> Option<Instant> {
        self.sessions.busy_since(chat)
    }

    pub fn stop(&self, chat: &str) {
        self.sessions.stop(chat);
    }

    /// Sends a Pi-style mid-turn steering message to a running chat.
    pub fn steer(&self, chat: &str, message: Value) -> bool {
        self.sessions.steer(chat, message)
    }

    /// Stops `chat` and marks it deleted so an active turn won't save its file back to disk.
    pub fn stop_and_delete(&self, chat: &str) {
        self.sessions.stop_and_delete(chat);
    }

    /// Stops every active turn for `project` and marks them deleted.
    pub fn stop_project(&self, project: &str) {
        self.sessions.stop_project(project);
    }

    /// Stops every active turn (when the app quits).
    pub fn stop_all(&self) {
        self.sessions.stop_all();
    }

    pub fn answer(&self, chat: &str, call: &str, allow: bool) {
        self.sessions.answer(chat, call, allow);
    }
}

/// Builds an OpenAI-format user message: plain string when text-only, or multimodal content array
/// with `image_url` data URIs (and an optional local-paths metadata part) when images are attached.
pub fn user_message(
    text: &str,
    images: &[crate::attachments::ImageAttachment],
    image_paths_note: Option<&str>,
) -> Value {
    if images.is_empty() {
        return json!({ "role": "user", "content": text });
    }
    let mut parts: Vec<Value> = Vec::with_capacity(images.len() + 2);
    if !text.is_empty() {
        parts.push(json!({ "type": "text", "text": text }));
    }
    if let Some(note) = image_paths_note.filter(|s| !s.is_empty()) {
        parts.push(json!({
            "type": "text",
            "text": note,
            "superviser_meta": true,
        }));
    }
    for img in images {
        parts.push(json!({
            "type": "image_url",
            "image_url": { "url": img.data_url() },
        }));
    }
    json!({ "role": "user", "content": parts })
}

/// Adds the user's message (with optional image attachments) to a chat (a new one without `chat`)
/// and starts the harness on it. If `chat` is already running, queues the message as a Pi-style
/// mid-turn steer so the model adapts without aborting.
pub fn send(
    app: Arc<App>,
    project: &str,
    chat: Option<&str>,
    text: &str,
    images: &[crate::attachments::ImageAttachment],
    model: &str,
) -> anyhow::Result<String> {
    if images.len() > crate::attachments::MAX_IMAGES_PER_MESSAGE {
        bail!("Too many images (max {}).", crate::attachments::MAX_IMAGES_PER_MESSAGE);
    }
    for img in images {
        img.validate()?;
    }
    if text.trim().is_empty() && images.is_empty() {
        bail!("Enter a message or attach an image.");
    }
    let project = app.project(project).context("that project no longer exists")?;
    let include_paths = app.settings.lock().unwrap().include_image_paths;
    let path_note = include_paths
        .then(|| crate::attachments::format_image_paths_note(images, Some(&project)))
        .flatten();
    let message = user_message(text, images, path_note.as_deref());

    // Pi-style mid-turn steering: if this chat is already busy, inject the guidance mid-loop!
    if let Some(id) = chat
        && app.chats.busy(id)
        && app.chats.steer(id, message.clone())
    {
        return Ok(id.to_string());
    }

    let (loaded_model, context_window) = app.loaded(model).context("Load the chat's model first.")?;
    app.touch(&loaded_model.id);

    let store = ChatStore::in_data_dir(&app.paths.data);
    let mut chat = match chat {
        Some(id) => store.load(&project.id, id)?,
        None => Chat::new(&project.id),
    };
    let (stopped, steer_rx) = app.chats.sessions.begin_turn(&chat.id, &project.id)?;

    chat.messages.push(message.clone());
    if let Err(e) = store.save(&mut chat) {
        app.chats.sessions.end_turn(&chat.id);
        return Err(e);
    }
    let id = chat.id.clone();
    app.bus.emit(AppEvent::Chat(ChatEvent::State {
        chat: id.clone(),
        busy: true,
        error: None,
    }));
    app.bus.emit(AppEvent::Chat(ChatEvent::Message {
        chat: id.clone(),
        message,
    }));

    let sync_app = app.clone();
    tokio::spawn(async move {
        sync_app.sync_upstreams().await;
    });

    tokio::spawn(async move {
        let _awake = Awake::hold();
        let result = run_project_turn(
            &app,
            &project,
            (&loaded_model.alias, context_window),
            store,
            &mut chat,
            stopped,
            steer_rx,
        )
        .await;
        app.chats.sessions.end_turn(&chat.id);
        app.bus.emit(AppEvent::Chat(ChatEvent::State {
            chat: chat.id.clone(),
            busy: false,
            error: result.err().map(|e| format!("{e:#}")),
        }));
    });
    Ok(id)
}

/// Explicitly compacts older turns of an idle chat (`/compact`).
pub async fn compact(app: &App, project_id: &str, chat_id: &str, model: &str) -> anyhow::Result<Option<String>> {
    if app.chats.busy(chat_id) {
        bail!("Wait for the current turn to finish before compacting.");
    }
    let project = app.project(project_id).context("that project no longer exists")?;
    let (loaded_model, context_window) = app.loaded(model).context("Load a model first to compact this chat.")?;
    let guard = app.guard_url().context("the guard is not running")?;
    let endpoint = ModelEndpoint {
        completions_url: format!("{}{}/v1/chat/completions", guard.trim_end_matches("/v1"), project.guard_path()),
        api_key: secrets::local_api_key(&app.paths)?,
        model: loaded_model.alias,
        context_window,
    };
    let store = ChatStore::in_data_dir(&app.paths.data);
    let mut chat = store.load(&project.id, chat_id)?;
    let summary = compact_chat(&store, &endpoint, &mut chat, 4).await?;
    if summary.is_some() {
        app.bus.emit(AppEvent::Chat(ChatEvent::Notice {
            chat: chat.id.clone(),
            text: "Compacted earlier turns into a summary checkpoint.".into(),
        }));
    }
    Ok(summary)
}

/// Creates a branched copy of `chat_id` keeping the first `keep_messages` messages.
pub fn fork(app: &App, project_id: &str, chat_id: &str, keep_messages: usize) -> anyhow::Result<Chat> {
    let store = ChatStore::in_data_dir(&app.paths.data);
    store.fork(project_id, chat_id, keep_messages)
}

struct IsolationRuntime {
    runner: Option<Arc<dyn CommandRunner>>,
    path_mapper: Option<Arc<dyn PathMapper>>,
    prompt_cfg: PromptConfig,
}

async fn run_project_turn(
    app: &Arc<App>,
    project: &Project,
    model_info: (&str, u64),
    store: ChatStore,
    chat: &mut Chat,
    stopped: watch::Receiver<bool>,
    steer_rx: tokio::sync::mpsc::UnboundedReceiver<Value>,
) -> anyhow::Result<()> {
    let (model_alias, context_window) = model_info;
    let main = std::fs::canonicalize(project.main())
        .with_context(|| format!("{} is gone", project.main().display()))?;
    let related: Vec<Folder> = project
        .related()
        .iter()
        .filter_map(|f| {
            Some(Folder {
                path: std::fs::canonicalize(&f.path).ok()?,
                writable: f.writable,
            })
        })
        .collect();
    let (commands, edits, allow_subagents) = (
        project.allows("commands"),
        project.allows("edits"),
        project.allows("subagents"),
    );

    let ws_folders: Vec<WorkspaceFolder> = std::iter::once(WorkspaceFolder {
        path: main.clone(),
        writable: true,
    })
    .chain(related.iter().map(|f| WorkspaceFolder {
        path: f.path.clone(),
        writable: f.writable,
    }))
    .collect();

    let IsolationRuntime { runner, path_mapper, mut prompt_cfg } = match project.isolation {
        Isolation::MacosSandbox => {
            let runner: Option<Arc<dyn CommandRunner>> = if commands {
                let profile = seatbelt::write_project_profile(
                    &app.paths,
                    &main,
                    &related,
                    project.network,
                    &project.blocked_programs,
                    &format!("chat-{}", project.id),
                )?;
                Some(Arc::new(SeatbeltRunner { profile, main: main.clone() }))
            } else {
                None
            };
            let mut cfg = PromptConfig::new(&project.name, main.clone());
            cfg.related = ws_folders[1..].to_vec();
            cfg.environment = "a macOS Seatbelt sandbox".into();
            cfg.shell_label = "zsh on macOS".into();
            IsolationRuntime { runner, path_mapper: None, prompt_cfg: cfg }
        }
        Isolation::LinuxVm => {
            let runner: Option<Arc<dyn CommandRunner>> = if commands {
                let paths = app.paths.clone();
                let main_c = main.clone();
                let rel_c = related.clone();
                let net = project.network;
                let bus = app.bus.clone();
                let cid = chat.id.clone();
                let vm = tokio::task::spawn_blocking(move || -> anyhow::Result<ShellVm> {
                    let vm = ShellVm::new(&paths, &main_c, &rel_c, net)?;
                    if !vm.has_image() {
                        bus.emit(AppEvent::Chat(ChatEvent::Notice {
                            chat: cid,
                            text: "Building the Linux sandbox image once (this takes a couple of minutes)…".into(),
                        }));
                        vm.ensure_image()?;
                    }
                    Ok(vm)
                })
                .await
                .context("Linux VM setup task failed")??;
                Some(Arc::new(LinuxVmRunner { vm: Arc::new(vm) }))
            } else {
                None
            };
            let (vm_main, vm_rel_paths) = container::vm_paths(&related);
            let mut cfg = PromptConfig::new(&project.name, main.clone());
            cfg.display_main = vm_main;
            cfg.display_related = related
                .iter()
                .zip(vm_rel_paths)
                .map(|(f, vm_p)| (vm_p, f.writable))
                .collect();
            cfg.environment = "an isolated Linux VM".into();
            cfg.shell_label = "bash in Linux VM".into();
            let mapper: Arc<dyn PathMapper> = Arc::new(VmPathMapper {
                main: main.clone(),
                related: related.clone(),
            });
            IsolationRuntime { runner, path_mapper: Some(mapper), prompt_cfg: cfg }
        }
    };

    prompt_cfg.commands = commands;
    prompt_cfg.edits = edits;
    prompt_cfg.subagents = allow_subagents;
    prompt_cfg.internet = project.network == Network::Internet;

    let mut workspace = Workspace::new(&ws_folders, runner, edits)?;
    if let Some(mapper) = path_mapper {
        workspace = workspace.with_path_mapper(mapper);
    }

    let guard = app.guard_url().context("the guard is not running")?;
    let endpoint = ModelEndpoint {
        completions_url: format!("{}{}/v1/chat/completions", guard.trim_end_matches("/v1"), project.guard_path()),
        api_key: secrets::local_api_key(&app.paths)?,
        model: model_alias.to_string(),
        context_window,
    };

    let subagents: Option<Arc<dyn SubagentHandler>> = allow_subagents.then(|| {
        Arc::new(BackendSubagentHandler {
            app: app.clone(),
            project: project.clone(),
            parent_model_alias: model_alias.to_string(),
            folders: ws_folders.clone(),
        }) as Arc<dyn SubagentHandler>
    });

    let approvals = match project.approvals {
        Approvals::Commands => ApprovalPolicy::Commands,
        Approvals::Everything => ApprovalPolicy::Everything,
        Approvals::Never => ApprovalPolicy::Never,
    };

    let sink = BusEventSink { bus: app.bus.clone() };
    let cfg = TurnConfig {
        store,
        endpoint,
        system_prompt: entropy::prompt::build(&prompt_cfg),
        workspace,
        approvals,
        subagents,
        max_steps: MAX_STEPS,
    };
    run_turn(&app.chats.sessions, &sink, cfg, chat, stopped, steer_rx).await
}

struct BusEventSink {
    bus: EventBus,
}

impl EventSink for BusEventSink {
    fn emit(&self, event: HarnessEvent) {
        let mapped = match event {
            HarnessEvent::Message { chat, message } => ChatEvent::Message { chat, message },
            HarnessEvent::Delta { chat, content, reasoning } => ChatEvent::Delta { chat, content, reasoning },
            HarnessEvent::Approval { chat, call, name, arguments } => {
                ChatEvent::Approval { chat, call, name, arguments }
            }
            HarnessEvent::Usage { chat, used, size } => ChatEvent::Usage { chat, used, size },
            HarnessEvent::Notice { chat, text } => ChatEvent::Notice { chat, text },
            HarnessEvent::State { chat, busy, error } => ChatEvent::State { chat, busy, error },
        };
        self.bus.emit(AppEvent::Chat(mapped));
    }
}

struct SeatbeltRunner {
    profile: PathBuf,
    main: PathBuf,
}

impl CommandRunner for SeatbeltRunner {
    fn shell_description(&self) -> &str {
        "zsh"
    }

    fn run(
        &self,
        command: &str,
        timeout: Duration,
        cancel: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, String>> + Send + '_>> {
        let cmd = seatbelt::shell(&self.profile, &self.main, command);
        Box::pin(run_process(cmd, timeout, cancel))
    }
}

struct LinuxVmRunner {
    vm: Arc<ShellVm>,
}

impl CommandRunner for LinuxVmRunner {
    fn shell_description(&self) -> &str {
        "bash in Linux VM"
    }

    fn run(
        &self,
        command: &str,
        timeout: Duration,
        cancel: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, String>> + Send + '_>> {
        let cmd = self.vm.shell_command(command);
        Box::pin(run_process(cmd, timeout, cancel))
    }
}

struct VmPathMapper {
    main: PathBuf,
    related: Vec<Folder>,
}

impl PathMapper for VmPathMapper {
    fn to_host(&self, path: &Path) -> Option<PathBuf> {
        container::host_path(&self.main, &self.related, path)
    }
}

struct BackendSubagentHandler {
    app: Arc<App>,
    project: Project,
    parent_model_alias: String,
    folders: Vec<WorkspaceFolder>,
}

impl SubagentHandler for BackendSubagentHandler {
    fn run(
        &self,
        chat_id: String,
        call: ToolCall,
        cancel: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = String> + Send + '_>> {
        Box::pin(run_subagent(
            &self.app,
            &self.project,
            chat_id,
            &self.parent_model_alias,
            &self.folders,
            call,
            cancel,
        ))
    }
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

async fn run_subagent(
    app: &App,
    project: &Project,
    parent_chat_id: String,
    parent_model_alias: &str,
    folders: &[WorkspaceFolder],
    call: ToolCall,
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
            parent_session: parent_chat_id,
            child_session: None,
            role: role.clone(),
            title,
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

fn summarize_builtin_call(call: &ToolCall) -> (String, String) {
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
) -> anyhow::Result<StreamReply> {
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
    let mut reply = StreamReply::default();
    let mut calls: Vec<ToolCall> = Vec::new();
    let mut buffer: Vec<u8> = Vec::new();
    let mut last = Instant::now();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        buffer.extend_from_slice(&chunk.context("the sub-agent reply was cut off")?);
        while let Some((start, end)) = entropy::engine::find_sse_event_end(&buffer) {
            let event: Vec<u8> = buffer.drain(..end).collect();
            entropy::engine::consume_sse_event(&event[..start], &mut reply, &mut calls)?;
        }
        if last.elapsed() >= Duration::from_millis(120) && !reply.content.is_empty() {
            app.subagents.update_with(sub_id, &app.bus, |rec| {
                rec.live_text = Some(reply.content.clone());
            });
            last = Instant::now();
        }
    }
    if !buffer.is_empty() {
        entropy::engine::consume_sse_event(&buffer, &mut reply, &mut calls)?;
    }
    for (i, call) in calls.iter_mut().enumerate() {
        if call.id.is_empty() {
            call.id = format!("subcall_{i}");
        }
    }
    reply.calls = calls.into_iter().filter(|c| !c.name.is_empty()).collect();
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_message_supports_plain_and_multimodal_images() {
        assert_eq!(user_message("hello", &[], None), json!({ "role": "user", "content": "hello" }));
        let img = crate::attachments::ImageAttachment::from_bytes(
            "shot.png".into(),
            "image/png",
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
        )
        .unwrap();
        let msg = user_message(
            "describe this",
            std::slice::from_ref(&img),
            Some("[Attached image file paths]\n- shot.png: /tmp/shot.png"),
        );
        let parts = msg["content"].as_array().unwrap();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], json!({ "type": "text", "text": "describe this" }));
        assert_eq!(parts[1]["superviser_meta"], true);
        assert_eq!(parts[2]["type"], "image_url");
        assert_eq!(parts[2]["image_url"]["url"], img.data_url());

        let clean = entropy::context::for_model(&msg);
        assert!(clean["content"][1].get("superviser_meta").is_none());

        let img_only = user_message("", std::slice::from_ref(&img), None);
        assert_eq!(img_only["content"].as_array().unwrap().len(), 1);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn seatbelt_runner_enforces_sandbox() {
        let root = std::env::temp_dir().join(format!("superviser-seatbelt-runner-{}", std::process::id()));
        let main = root.join("app");
        std::fs::create_dir_all(&main).unwrap();
        let paths = superviser_core::paths::Paths::at(root.join("data"));
        let main = std::fs::canonicalize(&main).unwrap();
        let profile = seatbelt::write_project_profile(
            &paths,
            &main,
            &[],
            Network::LocalOnly,
            &["uname".into()],
            "test",
        )
        .unwrap();
        let runner: Arc<dyn CommandRunner> = Arc::new(SeatbeltRunner { profile, main: main.clone() });
        let ws = Workspace::new(
            &[WorkspaceFolder { path: main, writable: true }],
            Some(runner),
            true,
        )
        .unwrap();
        let (_tx, cancel) = watch::channel(false);
        assert_eq!(
            ws.run("bash", r#"{"command": "echo hi > made.txt && cat made.txt"}"#, cancel.clone()).await,
            "hi\n(exit code 0)"
        );
        let blocked = ws.run("bash", r#"{"command": "uname"}"#, cancel).await;
        assert!(!blocked.contains("Darwin") && !blocked.contains("exit code 0"), "{blocked}");
        std::fs::remove_dir_all(root).ok();
    }
}
