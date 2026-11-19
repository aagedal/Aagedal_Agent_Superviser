//! The chat window's engine: OpenCode's Agent Client Protocol server (`opencode acp`), one
//! per project, run in the project's macOS sandbox and spoken to over its stdin and stdout
//! (JSON-RPC, a message per line). It reaches the model through the guard on the project's
//! path, with every provider but ours turned off. What it reports goes to the window as
//! `acp` events, and its permission requests wait there for the user's answer.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use serde::Serialize;
use serde_json::{Value, json};
use superviser_core::project::{Folder, Project};
use superviser_core::sandbox::{Isolation, seatbelt};
use superviser_core::{opencode, secrets};
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::ChildStdin;
use tokio::sync::oneshot;

use crate::state::App;

/// How long requests other than prompts may take (OpenCode's first start takes a few seconds).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const PROTOCOL_VERSION: u64 = 1;

/// What the chat window hears, on the `acp` event.
#[derive(Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Event {
    /// A `session/update` from OpenCode, as it sent it.
    Update { project: String, session: String, update: Value },
    /// OpenCode asks before a call; answer with `acp_permission`.
    Permission { project: String, session: String, request: String, tool_call: Value, options: Value },
    /// A prompt started or ended.
    State { project: String, session: String, busy: bool, stop_reason: Option<String>, error: Option<String> },
}

/// The running engines, by project id.
#[derive(Default)]
pub struct Engines {
    running: tokio::sync::Mutex<HashMap<String, Arc<Engine>>>,
}

pub struct Engine {
    project: String,
    /// What it was started with; a project or model change means a new one.
    fingerprint: String,
    stdin: tokio::sync::Mutex<ChildStdin>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    /// Permission requests waiting for the user: our id → (the JSON-RPC id to answer, session).
    asks: Mutex<HashMap<String, (Value, String)>>,
    /// Sessions with a prompt in flight.
    busy: Mutex<HashSet<String>>,
    alive: AtomicBool,
    pid: Option<u32>,
}

impl Engines {
    /// The engine for `project`, started (or restarted, when its settings or the model changed
    /// and it's idle) as needed.
    pub async fn get(&self, handle: &AppHandle, app: &App, project: &Project) -> anyhow::Result<Arc<Engine>> {
        if project.isolation != Isolation::MacosSandbox {
            bail!("The chat runs projects in the macOS sandbox so far. Open this one in OpenCode, or switch it to the macOS sandbox in Projects.");
        }
        let launch = Launch::new(app, project)?;
        let mut running = self.running.lock().await;
        if let Some(engine) = running.get(&project.id)
            && engine.alive.load(Ordering::SeqCst)
            && (engine.fingerprint == launch.fingerprint || !engine.busy.lock().unwrap().is_empty())
        {
            return Ok(engine.clone());
        }
        if let Some(old) = running.remove(&project.id) {
            old.stop();
        }
        let engine = Engine::start(handle.clone(), &project.id, launch).await?;
        running.insert(project.id.clone(), engine.clone());
        Ok(engine)
    }

    /// The engine for a project if one runs.
    pub async fn running(&self, project: &str) -> Option<Arc<Engine>> {
        self.running.lock().await.get(project).filter(|e| e.alive.load(Ordering::SeqCst)).cloned()
    }

    /// Stops every engine (when the app quits).
    pub fn stop_all(&self) {
        if let Ok(mut running) = self.running.try_lock() {
            for (_, engine) in running.drain() {
                engine.stop();
            }
        }
    }
}

/// How to start OpenCode for a project.
struct Launch {
    command: std::process::Command,
    fingerprint: String,
}

impl Launch {
    fn new(app: &App, project: &Project) -> anyhow::Result<Self> {
        // Without a model it can still list and replay saved chats; it starts again once one runs.
        let model = app.running_model();
        let guard = app.guard_url().context("the guard is not running")?;
        let binary = opencode::binary().context("OpenCode isn't bundled with this build and isn't installed")?;
        let main = std::fs::canonicalize(project.main()).with_context(|| format!("{} is gone", project.main().display()))?;
        let related: Vec<Folder> = project.related().iter().filter_map(|f| Some(Folder { path: std::fs::canonicalize(&f.path).ok()?, writable: f.writable })).collect();
        secrets::local_api_key(&app.paths)?;
        let setup = opencode::Setup {
            base_url: format!("{}{}/v1", guard.trim_end_matches("/v1"), project.guard_path()),
            api_key_file: app.paths.local_api_key(),
            models: model.iter().map(|(m, context)| opencode::SetupModel { alias: m.alias.clone(), name: m.model.name.clone(), context: *context }).collect(),
            default_model: model.as_ref().map(|(m, _)| m.alias.clone()),
        };
        // The project's rules, and our provider as the only one.
        let mut config = project.opencode_config(&opencode::mcp_servers());
        if let (Some(config), Value::Object(ours)) = (config.as_object_mut(), opencode::only_ours(&setup)) {
            config.extend(ours);
        }
        let fingerprint = json!({ "project": project, "config": config, "binary": binary }).to_string();
        let profile = seatbelt::write_project_profile(&app.paths, &main, &related, project.network, &project.blocked_programs, &format!("acp-{}", project.id))?;
        let mut command = seatbelt::sandboxed(&profile, &main, &binary, &["acp"]);
        command.env("OPENCODE_CONFIG_CONTENT", config.to_string());
        // Its own process group, so stopping it stops the shells it started.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let log = std::fs::OpenOptions::new().create(true).append(true).open(app.paths.logs().join("opencode-acp.log"))?;
        command.stderr(log);
        Ok(Self { command, fingerprint })
    }
}

impl Engine {
    async fn start(handle: AppHandle, project: &str, launch: Launch) -> anyhow::Result<Arc<Self>> {
        let mut child = tokio::process::Command::from(launch.command)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("couldn't start OpenCode")?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let engine = Arc::new(Self {
            project: project.to_string(),
            fingerprint: launch.fingerprint,
            stdin: tokio::sync::Mutex::new(stdin),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            asks: Mutex::new(HashMap::new()),
            busy: Mutex::new(HashSet::new()),
            alive: AtomicBool::new(true),
            pid: child.id(),
        });
        let reader = engine.clone();
        tauri::async_runtime::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match serde_json::from_str::<Value>(&line) {
                    Ok(message) => reader.receive(&handle, message).await,
                    Err(_) => tracing::debug!("opencode acp: {line}"),
                }
            }
            reader.alive.store(false, Ordering::SeqCst);
            // Whoever still waits gets an answer.
            for (_, waiting) in reader.pending.lock().unwrap().drain() {
                let _ = waiting.send(Err("OpenCode stopped; see the opencode-acp log".into()));
            }
            let _ = child.wait().await;
        });
        let init = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "clientCapabilities": {},
            "clientInfo": { "name": "Agent Superviser", "version": env!("CARGO_PKG_VERSION") },
        });
        engine.request("initialize", init, Some(REQUEST_TIMEOUT)).await.context("OpenCode didn't start")?;
        Ok(engine)
    }

    fn stop(&self) {
        self.alive.store(false, Ordering::SeqCst);
        if let Some(pid) = self.pid {
            let _ = std::process::Command::new("/bin/kill").args(["-TERM", &format!("-{pid}")]).status();
        }
    }

    async fn send(&self, message: Value) -> anyhow::Result<()> {
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(format!("{message}\n").as_bytes()).await.context("OpenCode stopped")?;
        stdin.flush().await?;
        Ok(())
    }

    /// Sends a request and waits for its result (at most `timeout`, when given).
    async fn request(&self, method: &str, params: Value, timeout: Option<Duration>) -> anyhow::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).await?;
        let answer = match timeout {
            Some(limit) => tokio::time::timeout(limit, rx).await.map_err(|_| anyhow::anyhow!("OpenCode didn't answer {method} in time"))?,
            None => rx.await,
        };
        answer.map_err(|_| anyhow::anyhow!("OpenCode stopped"))?.map_err(|e| anyhow::anyhow!("{e}"))
    }

    async fn notify(&self, method: &str, params: Value) -> anyhow::Result<()> {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params })).await
    }

    async fn receive(&self, handle: &AppHandle, message: Value) {
        let method = message["method"].as_str().map(str::to_string);
        match (method.as_deref(), message.get("id")) {
            // A response to one of our requests.
            (None, Some(id)) => {
                if let Some(waiting) = id.as_u64().and_then(|id| self.pending.lock().unwrap().remove(&id)) {
                    let result = match message.get("error") {
                        Some(error) => Err(error["message"].as_str().unwrap_or("OpenCode reported an error").to_string()),
                        None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = waiting.send(result);
                }
            }
            (Some("session/update"), None) => {
                let params = &message["params"];
                let _ = handle.emit(
                    "acp",
                    Event::Update { project: self.project.clone(), session: params["sessionId"].as_str().unwrap_or("").into(), update: params["update"].clone() },
                );
            }
            (Some("session/request_permission"), Some(id)) => {
                let params = &message["params"];
                let request = format!("{}-{}", self.project, id);
                let session = params["sessionId"].as_str().unwrap_or("").to_string();
                self.asks.lock().unwrap().insert(request.clone(), (id.clone(), session));
                let _ = handle.emit(
                    "acp",
                    Event::Permission {
                        project: self.project.clone(),
                        session: params["sessionId"].as_str().unwrap_or("").into(),
                        request,
                        tool_call: params["toolCall"].clone(),
                        options: params["options"].clone(),
                    },
                );
            }
            // We offer no file system or terminal of our own: OpenCode does those itself, in the sandbox.
            (Some(method), Some(id)) => {
                let error = json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("{method} isn't supported") } });
                let _ = self.send(error).await;
            }
            _ => {}
        }
    }

    /// The project's saved sessions, most recent first.
    pub async fn sessions(&self, cwd: &str) -> anyhow::Result<Value> {
        let result = self.request("session/list", json!({ "cwd": cwd }), Some(REQUEST_TIMEOUT)).await?;
        Ok(result["sessions"].clone())
    }

    /// A new session in the project's folders; returns its id.
    pub async fn new_session(&self, project: &Project) -> anyhow::Result<String> {
        let params = json!({
            "cwd": project.main(),
            "mcpServers": [],
            "additionalDirectories": project.related().iter().map(|f| &f.path).collect::<Vec<_>>(),
        });
        let result = self.request("session/new", params, Some(REQUEST_TIMEOUT)).await?;
        result["sessionId"].as_str().map(str::to_string).context("OpenCode didn't name the session")
    }

    /// Opens a saved session; OpenCode replays it as updates.
    pub async fn load_session(&self, project: &Project, session: &str) -> anyhow::Result<()> {
        let params = json!({ "sessionId": session, "cwd": project.main(), "mcpServers": [] });
        self.request("session/load", params, Some(REQUEST_TIMEOUT)).await?;
        Ok(())
    }

    pub async fn delete_session(&self, session: &str) -> anyhow::Result<()> {
        self.request("session/delete", json!({ "sessionId": session }), Some(REQUEST_TIMEOUT)).await?;
        Ok(())
    }

    pub fn is_busy(&self, session: &str) -> bool {
        self.busy.lock().unwrap().contains(session)
    }

    /// Sends the user's message; the reply arrives as events, and the end as a `state` event.
    pub fn prompt(self: &Arc<Self>, handle: AppHandle, session: &str, text: &str) -> anyhow::Result<()> {
        if !self.busy.lock().unwrap().insert(session.to_string()) {
            bail!("The agent is still working on this chat.");
        }
        let state = |busy: bool, stop_reason: Option<String>, error: Option<String>| Event::State {
            project: self.project.clone(),
            session: session.to_string(),
            busy,
            stop_reason,
            error,
        };
        let _ = handle.emit("acp", state(true, None, None));
        let engine = self.clone();
        let (session, text) = (session.to_string(), text.to_string());
        tauri::async_runtime::spawn(async move {
            let params = json!({ "sessionId": session, "prompt": [{ "type": "text", "text": text }] });
            let result = engine.request("session/prompt", params, None).await;
            engine.busy.lock().unwrap().remove(&session);
            let event = match result {
                Ok(result) => Event::State {
                    project: engine.project.clone(),
                    session,
                    busy: false,
                    stop_reason: result["stopReason"].as_str().map(str::to_string),
                    error: None,
                },
                Err(e) => Event::State { project: engine.project.clone(), session, busy: false, stop_reason: None, error: Some(format!("{e:#}")) },
            };
            let _ = handle.emit("acp", event);
        });
        Ok(())
    }

    /// Asks OpenCode to stop the session's prompt; it ends with stop reason `cancelled`.
    pub async fn cancel(&self, session: &str) -> anyhow::Result<()> {
        // Questions still open are answered as cancelled, as the protocol asks.
        let open: Vec<Value> = {
            let mut asks = self.asks.lock().unwrap();
            let keys: Vec<String> = asks.iter().filter(|(_, (_, s))| s == session).map(|(k, _)| k.clone()).collect();
            keys.into_iter().filter_map(|k| asks.remove(&k)).map(|(id, _)| id).collect()
        };
        for id in open {
            self.send(json!({ "jsonrpc": "2.0", "id": id, "result": { "outcome": { "outcome": "cancelled" } } })).await?;
        }
        self.notify("session/cancel", json!({ "sessionId": session })).await
    }

    /// Answers a permission request with one of its options, or `None` to cancel it.
    pub async fn answer(&self, request: &str, option: Option<&str>) -> anyhow::Result<()> {
        let (id, _) = self.asks.lock().unwrap().remove(request).context("that question was already answered")?;
        let outcome = match option {
            Some(option) => json!({ "outcome": "selected", "optionId": option }),
            None => json!({ "outcome": "cancelled" }),
        };
        self.send(json!({ "jsonrpc": "2.0", "id": id, "result": { "outcome": outcome } })).await
    }
}
