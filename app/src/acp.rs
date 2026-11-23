//! The chat window's engine: OpenCode's Agent Client Protocol server (`opencode acp`), one
//! per project, run in the project's macOS sandbox or Linux VM and spoken to over its stdin
//! and stdout (JSON-RPC, a message per line). It reaches the model through the guard on the
//! project's path, with every provider but ours turned off. What it reports goes to the window as
//! `acp` events, and its permission requests wait there for the user's answer.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use serde::Serialize;
use serde_json::{Value, json};
use superviser_core::awake::Awake;
use superviser_core::project::{Folder, Project};
use superviser_core::sandbox::container::{self, Use, Vm};
use superviser_core::sandbox::{self, Isolation, seatbelt};
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
    /// OpenCode is starting slowly for the project: why.
    Starting { project: String, text: String },
}

/// The running engines, by project id.
#[derive(Default)]
pub struct Engines {
    /// Sync mutex (never held across `.await`) so `running()` and `stop_all()` never block on VM builds.
    running: Mutex<HashMap<String, Arc<Engine>>>,
    /// Per-project startup gate so starting/building one project never blocks other projects.
    starting: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
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
    /// Sessions this process has open (made or loaded); others must be loaded before a prompt.
    open: Mutex<HashSet<String>>,
    /// Sessions being loaded only to prompt them: their replay isn't shown again.
    quiet: Mutex<HashSet<String>>,
    /// The project's main folder and its others, where OpenCode sees them.
    cwd: String,
    folders: Vec<String>,
    alive: AtomicBool,
    /// Child process group id while running, `0` once reaped.
    pid: AtomicU32,
    /// The VM it runs in (`container` and the VM's name), stopped with it.
    vm: Option<(PathBuf, String)>,
}

impl Engines {
    fn gate(&self, project: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.starting
            .lock()
            .unwrap()
            .entry(project.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// The engine for `project`, started (or restarted, when its settings or the model changed
    /// and it's idle) as needed.
    pub async fn get(&self, handle: &AppHandle, app: &App, project: &Project) -> anyhow::Result<Arc<Engine>> {
        let launch = Launch::new(app, project)?;
        if let Some(engine) = self.running.lock().unwrap().get(&project.id).cloned()
            && engine.alive.load(Ordering::SeqCst)
            && (engine.fingerprint == launch.fingerprint || !engine.busy.lock().unwrap().is_empty())
        {
            return Ok(engine);
        }
        let gate = self.gate(&project.id);
        let _guard = gate.lock().await;
        // Re-check after acquiring this project's startup gate.
        if let Some(engine) = self.running.lock().unwrap().get(&project.id).cloned()
            && engine.alive.load(Ordering::SeqCst)
            && (engine.fingerprint == launch.fingerprint || !engine.busy.lock().unwrap().is_empty())
        {
            return Ok(engine);
        }
        let old = self.running.lock().unwrap().remove(&project.id);
        if let Some(old) = old
            && let Some(vm_stop) = old.stop()
        {
            // Wait for the old VM with the same name to finish stopping before starting the replacement.
            let _ = tauri::async_runtime::spawn_blocking(move || vm_stop.join()).await;
        }
        let engine = Engine::start(handle.clone(), app, project, launch).await?;
        self.running.lock().unwrap().insert(project.id.clone(), engine.clone());
        Ok(engine)
    }

    /// The engine for a project if one runs.
    pub async fn running(&self, project: &str) -> Option<Arc<Engine>> {
        self.running.lock().unwrap().get(project).filter(|e| e.alive.load(Ordering::SeqCst)).cloned()
    }

    /// Stops one project's engine (e.g. when the project is deleted).
    pub fn stop(&self, project: &str) {
        if let Some(engine) = self.running.lock().unwrap().remove(project) {
            let _ = engine.stop();
        }
        self.starting.lock().unwrap().remove(project);
    }

    /// Stops every engine (when the app quits), waiting for their VMs to stop.
    pub fn stop_all(&self) {
        let engines: Vec<Arc<Engine>> = self.running.lock().unwrap().drain().map(|(_, e)| e).collect();
        let stopping: Vec<_> = engines.into_iter().filter_map(|engine| engine.stop()).collect();
        for vm in stopping {
            let _ = vm.join();
        }
    }
}

/// How to start OpenCode for a project. Cheap to make: what takes time happens in [`Launch::command`].
struct Launch {
    /// The project's rules, and our provider as the only one.
    config: Value,
    fingerprint: String,
    main: PathBuf,
    related: Vec<Folder>,
}

impl Launch {
    fn new(app: &App, project: &Project) -> anyhow::Result<Self> {
        let guard = app.guard_url().context("the guard is not running")?;
        let main = std::fs::canonicalize(project.main()).with_context(|| format!("{} is gone", project.main().display()))?;
        let related: Vec<Folder> = project.related().iter().filter_map(|f| Some(Folder { path: std::fs::canonicalize(&f.path).ok()?, writable: f.writable })).collect();
        let api_key = secrets::local_api_key(&app.paths)?;
        // Every model a chat might use, so a session's recorded model always resolves.
        let models = app
            .installed()
            .into_iter()
            .filter_map(|m| Some(opencode::SetupModel { context: app.fit(&m.model)?.context, alias: m.alias, name: m.model.name }))
            .collect();
        // No default model: each prompt names the chat's, so switching models needs no restart.
        let setup = match project.isolation {
            Isolation::MacosSandbox => opencode::Setup {
                base_url: format!("{}{}/v1", guard.trim_end_matches("/v1"), project.guard_path()),
                api_key_file: app.paths.local_api_key(),
                models,
                default_model: None,
            },
            Isolation::LinuxVm => container::provider_setup(&project.guard_path(), models, None),
        };
        let mut config = project.opencode_config(&opencode::mcp_servers());
        if let (Some(config), Value::Object(ours)) = (config.as_object_mut(), opencode::only_ours(&setup)) {
            config.extend(ours);
        }
        let binary = opencode::binary();
        let fingerprint = json!({ "project": project, "config": config, "binary": binary, "key": api_key }).to_string();
        Ok(Self { config, fingerprint, main, related })
    }

    /// Where OpenCode sees the main folder, and the others.
    fn folders(&self, project: &Project) -> (String, Vec<String>) {
        match project.isolation {
            Isolation::MacosSandbox => (self.main.display().to_string(), self.related.iter().map(|f| f.path.display().to_string()).collect()),
            Isolation::LinuxVm => container::vm_paths(&self.related),
        }
    }

    /// The command that starts OpenCode, isolated as the project says, and the VM it runs
    /// in, if one. A VM's first start builds its image, which takes a few minutes.
    async fn command(&self, handle: &AppHandle, app: &App, project: &Project) -> anyhow::Result<(std::process::Command, Option<(PathBuf, String)>)> {
        std::fs::create_dir_all(app.paths.logs())?;
        let log = std::fs::OpenOptions::new().create(true).append(true).open(app.paths.logs().join("opencode-acp.log"))?;
        let (mut command, vm) = match project.isolation {
            Isolation::MacosSandbox => {
                let binary = opencode::binary().context("OpenCode isn't bundled with this build and isn't installed")?;
                let profile = seatbelt::write_project_profile(&app.paths, &self.main, &self.related, project.network, &project.blocked_programs, &format!("acp-{}", project.id))?;
                let mut command = seatbelt::sandboxed(&profile, &self.main, &binary, &["acp"]);
                command.env("OPENCODE_CONFIG_CONTENT", self.config.to_string());
                (command, None)
            }
            Isolation::LinuxVm => {
                let guard = app.guard_url().context("the guard is not running")?;
                let (paths, owned, config, main, related) = (app.paths.clone(), project.clone(), self.config.clone(), self.main.clone(), self.related.clone());
                // Starting `container`'s services can take a while.
                let vm = tauri::async_runtime::spawn_blocking(move || -> anyhow::Result<Vm> {
                    let project = owned;
                    let backend = sandbox::backend(Isolation::LinuxVm);
                    backend.available().map_err(|why| anyhow::anyhow!("The Linux VM isn't available: {why}"))?;
                    backend.suits(&main).map_err(|why| anyhow::anyhow!("The Linux VM doesn't suit this project: {why}"))?;
                    let (guard_socket, guard_path) = (paths.guard_socket(), project.guard_path());
                    let request = sandbox::Request {
                        paths: &paths,
                        project: &main,
                        related: &related,
                        network: project.network,
                        guard_socket: &guard_socket,
                        model: None,
                        guard_url: &guard,
                        guard_path: &guard_path,
                        opencode: &config,
                        blocked_programs: &project.blocked_programs,
                    };
                    Vm::new(&request, &sandbox::stem(&project.id, &main), Use::Chat)
                })
                .await??;
                if !vm.has_image() {
                    let text = "Building the Linux environment. This happens once and takes a few minutes…".to_string();
                    let _ = handle.emit("acp", Event::Starting { project: project.id.clone(), text });
                    let status = tokio::process::Command::from(vm.build_command()).stdout(log.try_clone()?).stderr(log.try_clone()?).status().await?;
                    anyhow::ensure!(status.success(), "Building the Linux environment failed; see the opencode-acp log.");
                }
                let name = vm.name(Use::Chat);
                (vm.command(Use::Chat, &["opencode", "acp"]), Some((vm.container.clone(), name)))
            }
        };
        // Its own process group, so stopping it stops the shells it started.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        command.stderr(log);
        Ok((command, vm))
    }
}

impl Engine {
    async fn start(handle: AppHandle, app: &App, project: &Project, launch: Launch) -> anyhow::Result<Arc<Self>> {
        let (command, vm) = launch.command(&handle, app, project).await?;
        let (cwd, folders) = launch.folders(project);
        let mut child = tokio::process::Command::from(command)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("couldn't start OpenCode")?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let engine = Arc::new(Self {
            project: project.id.clone(),
            open: Mutex::new(HashSet::new()),
            quiet: Mutex::new(HashSet::new()),
            cwd,
            folders,
            fingerprint: launch.fingerprint,
            stdin: tokio::sync::Mutex::new(stdin),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            asks: Mutex::new(HashMap::new()),
            busy: Mutex::new(HashSet::new()),
            alive: AtomicBool::new(true),
            pid: AtomicU32::new(child.id().unwrap_or(0)),
            vm,
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
            reader.asks.lock().unwrap().clear();
            let _ = child.wait().await;
            reader.pid.store(0, Ordering::SeqCst);
        });
        let init = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "clientCapabilities": {},
            "clientInfo": { "name": "Agent Superviser", "version": env!("CARGO_PKG_VERSION") },
        });
        if let Err(e) = engine.request("initialize", init, Some(REQUEST_TIMEOUT)).await.context("OpenCode didn't start") {
            if let Some(vm_stop) = engine.stop() {
                let _ = tauri::async_runtime::spawn_blocking(move || vm_stop.join()).await;
            }
            return Err(e);
        }
        Ok(engine)
    }

    /// Stops OpenCode, and its VM in the background (join to wait for it).
    fn stop(&self) -> Option<std::thread::JoinHandle<()>> {
        self.alive.store(false, Ordering::SeqCst);
        let pid = self.pid.swap(0, Ordering::SeqCst);
        if pid != 0 {
            let _ = std::process::Command::new("/bin/kill").args(["-TERM", &format!("-{pid}")]).status();
        }
        let (container, name) = self.vm.clone()?;
        Some(std::thread::spawn(move || container::stop(&container, &name)))
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
        if let Err(e) = self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).await {
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        let answer = match timeout {
            Some(limit) => match tokio::time::timeout(limit, rx).await {
                Ok(answer) => answer,
                Err(_) => {
                    self.pending.lock().unwrap().remove(&id);
                    bail!("OpenCode didn't answer {method} in time");
                }
            },
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
                if self.quiet.lock().unwrap().contains(params["sessionId"].as_str().unwrap_or("")) {
                    return;
                }
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
    pub async fn sessions(&self) -> anyhow::Result<Value> {
        let result = self.request("session/list", json!({ "cwd": self.cwd }), Some(REQUEST_TIMEOUT)).await?;
        Ok(result["sessions"].clone())
    }

    /// A new session in the project's folders; returns its id.
    pub async fn new_session(&self) -> anyhow::Result<String> {
        let params = json!({ "cwd": self.cwd, "mcpServers": [], "additionalDirectories": self.folders });
        let result = self.request("session/new", params, Some(REQUEST_TIMEOUT)).await?;
        let session = result["sessionId"].as_str().map(str::to_string).context("OpenCode didn't name the session")?;
        self.open.lock().unwrap().insert(session.clone());
        Ok(session)
    }

    /// Opens a saved session; OpenCode replays it as updates.
    pub async fn load_session(&self, session: &str) -> anyhow::Result<()> {
        let params = json!({ "sessionId": session, "cwd": self.cwd, "mcpServers": [] });
        self.request("session/load", params, Some(REQUEST_TIMEOUT)).await?;
        self.open.lock().unwrap().insert(session.to_string());
        Ok(())
    }

    /// Loads `session` without showing its replay, unless this process has it open already
    /// (it doesn't after a restart for new project settings).
    async fn ensure_open(&self, session: &str) -> anyhow::Result<()> {
        if self.open.lock().unwrap().contains(session) {
            return Ok(());
        }
        self.quiet.lock().unwrap().insert(session.to_string());
        let params = json!({ "sessionId": session, "cwd": self.cwd, "mcpServers": [] });
        let loaded = self.request("session/load", params, Some(REQUEST_TIMEOUT)).await;
        self.quiet.lock().unwrap().remove(session);
        loaded?;
        self.open.lock().unwrap().insert(session.to_string());
        Ok(())
    }

    pub async fn delete_session(&self, session: &str) -> anyhow::Result<()> {
        self.request("session/delete", json!({ "sessionId": session }), Some(REQUEST_TIMEOUT)).await?;
        Ok(())
    }

    pub fn is_busy(&self, session: &str) -> bool {
        self.busy.lock().unwrap().contains(session)
    }

    /// Sends the user's message, to be answered by `model` (the running one's alias); the reply
    /// arrives as events, and the end as a `state` event.
    pub fn prompt(self: &Arc<Self>, handle: AppHandle, session: &str, text: &str, model: &str) -> anyhow::Result<()> {
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
        let (session, text, model) = (session.to_string(), text.to_string(), format!("{}/{model}", opencode::PROVIDER_ID));
        tauri::async_runtime::spawn(async move {
            // Including the tools it runs between requests to the model.
            let _awake = Awake::hold();
            let result = async {
                engine.ensure_open(&session).await?;
                // The session may have been made with another model; the one running answers.
                let choose = json!({ "sessionId": session, "configId": "model", "value": model });
                if let Err(e) = engine.request("session/set_config_option", choose, Some(REQUEST_TIMEOUT)).await {
                    tracing::warn!("choosing {model} for {session}: {e:#}");
                }
                let params = json!({ "sessionId": session, "prompt": [{ "type": "text", "text": text }] });
                engine.request("session/prompt", params, None).await
            }
            .await;
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
