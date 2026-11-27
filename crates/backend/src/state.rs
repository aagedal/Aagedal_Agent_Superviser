//! The app's state and what it can do, independent of windows and menus.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use superviser_core::awake::Awake;
use superviser_core::catalog::{self, Catalog, CatalogModel, Fit, KvType};
use superviser_core::conversations::{self, Conversations};
use superviser_core::discover::{self, Discovery, Locations};
use superviser_core::hardware::Hardware;
use superviser_core::hub::Hub;
use superviser_core::installed::{self, InstalledModel, Source};
use superviser_core::paths::{PROTECTED_NAMES, Paths};
use superviser_core::project::Project;
use superviser_core::runtime::{self, DEFAULT_SPEC_DRAFT_N_MAX, LlamaServer, ServerConfig, Speculative};
use superviser_core::sandbox::{self, Isolation, Network};
use superviser_core::{opencode, secrets};
use superviser_guard::journal::Journal;
use superviser_guard::selfprotect::SelfProtection;
use superviser_guard::{CallEvent, GuardServer, Judge, JudgeOptions, ProjectRules, ProxyOptions};

use crate::events::{AppEvent, DownloadEvent, EventBus};

pub const DEFAULT_GUARD_PORT: u16 = 8787;
/// The download id of the judge's model.
pub const JUDGE_DOWNLOAD: &str = "judge";
/// How downloads of the judge's model are named in the windows.
pub const JUDGE_NAME: &str = "the safety check model";

const MAX_EVENTS: usize = 200;
const LOAD_TIMEOUT: Duration = Duration::from_secs(600);
/// How long the Mac stays awake after a model call through the guard: long enough for an agent
/// in Terminal to run a command and call again.
const AWAKE_AFTER_CALL: Duration = Duration::from_secs(5 * 60);

/// Keeps the Mac awake for [`AWAKE_AFTER_CALL`] after it's dropped.
struct AwakeAfter(Option<Awake>);

impl Drop for AwakeAfter {
    fn drop(&mut self) {
        let awake = self.0.take();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                tokio::time::sleep(AWAKE_AFTER_CALL).await;
                drop(awake);
            });
        }
    }
}

/// Preferred reasoning depth for a model, injected by the guard on requests that don't specify one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningLevel {
    /// Keep the model's template default.
    #[default]
    Default,
    /// Disable thinking (`reasoning_effort = "none"`, `enable_thinking = false`).
    Off,
    /// Light thinking (`reasoning_effort = "low"`).
    Low,
    /// Balanced thinking (`reasoning_effort = "medium"`).
    Medium,
    /// Deep thinking (`reasoning_effort = "high"`).
    High,
}

impl ReasoningLevel {
    pub const ALL: &'static [Self] = &[Self::Default, Self::Off, Self::Low, Self::Medium, Self::High];

    pub fn api_value(self) -> Option<&'static str> {
        match self {
            Self::Default => None,
            Self::Off => Some("none"),
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High => Some("high"),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::Off => "Off",
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            Self::Default => "Think: Auto",
            Self::Off => "Think: Off",
            Self::Low => "Think: Low",
            Self::Medium => "Think: Med",
            Self::High => "Think: High",
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub guard_port: u16,
    /// Block flagged calls (true) or only log them.
    pub block: bool,
    /// Run the catalog's decision model beside the chat model, ask it whether WARN calls
    /// serve the task, and block those that don't. Off by default: it takes memory.
    pub judge: bool,
    /// Block WARN calls when the judge gives no answer in time.
    pub judge_fail_closed: bool,
    /// Write OpenCode's config whenever a model starts.
    pub configure_opencode: bool,
    /// Offer models other apps already downloaded (LM Studio, Ollama, Hugging Face cache…).
    pub use_other_apps: bool,
    pub last_model: Option<String>,
    /// Projects, most recently opened first.
    pub projects: Vec<Project>,
    /// The first-run guide was finished or skipped.
    pub setup_done: bool,
    /// Save each model's last conversation to disk when it's unloaded, and load it back when
    /// the model starts again.
    pub save_conversations: bool,
    /// Include local file paths alongside dragged & dropped images so the agent can inspect
    /// metadata or run local image tools on them inside the sandbox.
    #[serde(default = "default_true")]
    pub include_image_paths: bool,
    /// Remembered reasoning level per installed model id.
    #[serde(default)]
    pub model_reasoning: HashMap<String, ReasoningLevel>,
    /// Before projects: internet access for every folder, the folders opened, and how each
    /// was isolated. Read once to make projects of them, never written.
    #[serde(skip_serializing)]
    sandbox_network: Network,
    #[serde(skip_serializing)]
    recent_projects: Vec<String>,
    #[serde(skip_serializing)]
    project_isolation: HashMap<String, Isolation>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            guard_port: DEFAULT_GUARD_PORT,
            block: true,
            judge: false,
            judge_fail_closed: false,
            configure_opencode: false,
            use_other_apps: true,
            last_model: None,
            projects: Vec::new(),
            setup_done: false,
            save_conversations: true,
            include_image_paths: true,
            model_reasoning: HashMap::new(),
            sandbox_network: Network::default(),
            recent_projects: Vec::new(),
            project_isolation: HashMap::new(),
        }
    }
}

impl Settings {
    /// Makes projects of the folders opened before there were projects.
    fn migrate(&mut self) {
        for path in std::mem::take(&mut self.recent_projects) {
            if self.projects.iter().any(|p| p.main().display().to_string() == path) {
                continue;
            }
            let mut project = Project::new(path.clone().into());
            project.network = self.sandbox_network;
            project.isolation = self.project_isolation.get(&path).copied().unwrap_or_default();
            self.projects.push(project);
        }
        self.project_isolation.clear();
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum ServerStatus {
    Stopped,
    Loading {
        model: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        progress: Option<f32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        stage: Option<String>,
    },
    Running { model: String, name: String, context: u64 },
    Failed { model: String, error: String },
}

/// A model loaded (or loading, or failed to load), for Settings, the chat and the menu.
#[derive(Debug, Clone, Serialize)]
pub struct LoadedModel {
    pub id: String,
    pub name: String,
    pub alias: String,
    #[serde(flatten)]
    pub state: LoadState,
    /// Of the memory a model may take (weights, context, buffers).
    pub memory: u64,
    /// Active speculative decoding mode once running (e.g. `"MTP + n-gram"`, `"n-gram"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speculative: Option<String>,
    /// Of system memory, with the conversations it keeps.
    #[serde(skip)]
    ram: u64,
    /// Identifies the specific load attempt so an unload/reload race can't insert an old server.
    #[serde(skip)]
    load_id: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum LoadState {
    Loading {
        #[serde(skip_serializing_if = "Option::is_none")]
        progress: Option<f32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        stage: Option<String>,
    },
    Running { context: u64 },
    Failed { error: String },
}

/// What loading a model takes: whether it fits now, else which loaded models to unload
/// (least recently used first) to make room.
#[derive(Debug, Clone, Serialize)]
pub struct Room {
    pub fits: bool,
    pub unload: Vec<String>,
}

struct Running {
    server: LlamaServer,
    model: CatalogModel,
    /// What its conversation is saved with, if it may be.
    key: Option<conversations::Key>,
}

/// Where the judge stands, for Settings.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum JudgeStatus {
    /// Off, or on with no model running.
    #[default]
    Idle,
    Loading,
    Running,
    Failed { error: String },
}

pub struct App {
    pub paths: Paths,
    pub hardware: Hardware,
    pub catalog: Catalog,
    pub settings: Mutex<Settings>,
    pub guard: Mutex<Option<GuardServer>>,
    /// UI-agnostic event bus for Tauri and GPUI windows + tray controllers.
    pub bus: EventBus,
    /// The model servers, by installed model id.
    running: tokio::sync::Mutex<HashMap<String, Running>>,
    /// Loaded, loading and failed models, most recently used first.
    models: Mutex<Vec<LoadedModel>>,
    next_load_id: AtomicU64,
    /// PIDs of model servers currently starting (`wait_ready`), so quit can stop them too.
    loading_pids: Mutex<HashMap<u64, u32>>,
    judge_server: tokio::sync::Mutex<Option<LlamaServer>>,
    pub judge_status: Mutex<JudgeStatus>,
    /// Active download cancellation flags, by download id.
    pub downloads: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// Latest progress snapshot of every in-flight download, for newly opened windows.
    active_downloads: Mutex<HashMap<String, DownloadEvent>>,
    pub events: Mutex<VecDeque<CallEvent>>,
    /// Models other apps downloaded, from the last scan.
    pub discovery: Mutex<Discovery>,
    /// The built-in agent's turns in progress.
    pub chats: crate::agent::Chats,
    /// OpenCode, run headless for the chat window, by project.
    pub engines: crate::acp::Engines,
    /// First-class sub-agents triggered in OpenCode ACP or the built-in agent.
    pub subagents: Arc<crate::subagents::SubagentTracker>,
    /// The model each chat uses, by `project/chat`: switching a chat's model makes the next
    /// reply read the whole chat again, so each keeps its own.
    chat_models: Mutex<HashMap<String, String>>,
    /// Archived chats, as `project/chat`: kept, but out of the chat list.
    archived_chats: Mutex<BTreeSet<String>>,
    /// Each model's last conversation, on disk.
    pub conversations: Conversations,
}

impl App {
    pub fn new() -> anyhow::Result<Self> {
        let paths = Paths::new()?;
        superviser_core::paths::private_dir(&paths.data)?;
        // Model servers a killed or crashed earlier run left behind.
        runtime::stop_leftovers(&paths.run(), &paths.upstream_api_key().display().to_string());
        // And the chat's Linux VMs; `container` can be slow to answer, so not in the way.
        std::thread::spawn(superviser_core::sandbox::container::stop_chat_leftovers);
        let mut settings: Settings = std::fs::read_to_string(paths.settings())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        settings.migrate();
        let conversations = Conversations::new(paths.saved_conversations());
        if settings.save_conversations {
            let pruning = Conversations::new(paths.saved_conversations());
            std::thread::spawn(move || pruning.prune());
        } else {
            conversations.clear();
        }
        Ok(Self {
            conversations,
            hardware: Hardware::detect(),
            catalog: Catalog::bundled(),
            settings: Mutex::new(settings),
            guard: Mutex::new(None),
            bus: EventBus::new(),
            running: tokio::sync::Mutex::new(HashMap::new()),
            models: Mutex::new(Vec::new()),
            next_load_id: AtomicU64::new(1),
            loading_pids: Mutex::new(HashMap::new()),
            judge_server: tokio::sync::Mutex::new(None),
            judge_status: Mutex::new(JudgeStatus::Idle),
            downloads: Mutex::new(HashMap::new()),
            active_downloads: Mutex::new(HashMap::new()),
            events: Mutex::new(VecDeque::new()),
            discovery: Mutex::new(Discovery::default()),
            chats: crate::agent::Chats::default(),
            engines: crate::acp::Engines::default(),
            subagents: Arc::new(crate::subagents::SubagentTracker::default()),
            chat_models: Mutex::new(
                std::fs::read_to_string(paths.chat_models())
                    .ok()
                    .and_then(|t| serde_json::from_str(&t).ok())
                    .unwrap_or_default(),
            ),
            archived_chats: Mutex::new(
                std::fs::read_to_string(paths.archived_chats())
                    .ok()
                    .and_then(|t| serde_json::from_str(&t).ok())
                    .unwrap_or_default(),
            ),
            paths,
        })
    }

    pub fn hub() -> Hub {
        Hub::new(secrets::hf_token())
    }

    /// Whether to offer the setup guide: not finished or skipped, and not set up some other way
    /// (a model that fits on this Mac and a project, from Settings).
    pub fn needs_setup(&self) -> bool {
        let (done, projects) = {
            let settings = self.settings.lock().unwrap();
            (settings.setup_done, !settings.projects.is_empty())
        };
        !done && !(projects && self.installed().iter().any(|m| self.fit(&m.model).is_some()))
    }

    /// Whether `model` is loaded, or on its way.
    pub fn loading_or_loaded(&self, id: &str) -> bool {
        self.models()
            .iter()
            .any(|m| m.id == id && !matches!(m.state, LoadState::Failed { .. }))
    }

    /// Looks for models other apps downloaded (if allowed), replacing the last scan.
    pub fn rescan(&self) {
        let discovery = if self.settings.lock().unwrap().use_other_apps {
            discover::scan(&Locations::detect())
        } else {
            Discovery::default()
        };
        tracing::info!("found {} models from other apps", discovery.models.len());
        *self.discovery.lock().unwrap() = discovery;
    }

    /// Rescans other apps' caches and notifies subscribers.
    pub fn rescan_and_notify(&self) {
        self.rescan();
        self.bus.emit(AppEvent::Discovery);
    }

    /// The memory budgets chat models get: what the judge leaves when it is on.
    pub fn model_hardware(&self) -> Hardware {
        match &self.catalog.judge {
            Some(judge) if self.settings.lock().unwrap().judge => self.hardware.less(judge.judge_required()),
            _ => self.hardware.clone(),
        }
    }

    /// Whether the judge fits beside at least one curated model at its smallest context.
    pub fn judge_fits(&self) -> bool {
        self.catalog
            .judge
            .as_ref()
            .is_some_and(|judge| !self.catalog.offers(&self.hardware.less(judge.judge_required())).is_empty())
    }

    /// The judge's model, if downloaded.
    pub fn judge_installed(&self) -> Option<InstalledModel> {
        installed::find_judge(&self.paths, self.catalog.judge.as_ref()?)
    }

    /// Turns the judge on or off, and whether it fails closed. Turning it on while a model
    /// runs starts it now, if it fits beside that model at the context it has.
    pub async fn set_judge(&self, enabled: bool, fail_closed: bool) -> anyhow::Result<()> {
        let judge = self.catalog.judge.as_ref().context("this version has no judge model")?;
        if enabled && !self.settings.lock().unwrap().judge {
            let (used, _) = Self::used(&self.models.lock().unwrap(), "");
            if used + judge.judge_required() > self.hardware.model_budget {
                bail!(
                    "{} needs {:.1} GB beside the loaded models, which isn't free. Unload a model, turn the judge on, and load it again: it will get the context that fits.",
                    judge.name,
                    judge.judge_required() as f64 / 1e9,
                );
            }
        }
        {
            let mut settings = self.settings.lock().unwrap();
            settings.judge = enabled;
            settings.judge_fail_closed = fail_closed;
        }
        self.save_settings()?;
        self.sync_judge().await;
        self.bus.emit(AppEvent::Judge);
        Ok(())
    }

    /// Runs the judge while it is on, downloaded and a model is running; stops it otherwise.
    /// Options changes reach the guard without a restart.
    pub async fn sync_judge(&self) {
        let mut server = self.judge_server.lock().await;
        // Read desired state after acquiring the lock so queued syncs never act on stale settings.
        let (enabled, fail_closed) = {
            let settings = self.settings.lock().unwrap();
            (settings.judge, settings.judge_fail_closed)
        };
        let wanted = if enabled && self.latest_model().is_some() { self.judge_installed() } else { None };
        let Some(model) = wanted else {
            if let Some(mut old) = server.take() {
                self.remember_pid("judge", None);
                old.stop().await;
            }
            self.set_guard_judge(None);
            *self.judge_status.lock().unwrap() = JudgeStatus::Idle;
            self.bus.emit(AppEvent::Judge);
            return;
        };
        if server.as_mut().is_none_or(|s| !s.is_running()) {
            *self.judge_status.lock().unwrap() = JudgeStatus::Loading;
            self.bus.emit(AppEvent::Judge);
            match self.start_judge(&model).await {
                Ok(started) => {
                    *server = Some(started);
                }
                Err(e) => {
                    tracing::warn!("judge: {e:#}");
                    *server = None;
                    self.set_guard_judge(None);
                    *self.judge_status.lock().unwrap() = JudgeStatus::Failed { error: format!("{e:#}") };
                    self.bus.emit(AppEvent::Judge);
                    return;
                }
            }
        }
        let options = JudgeOptions { fail_closed, ..JudgeOptions::default() };
        let base = server.as_ref().unwrap().base_url();
        let key = secrets::upstream_api_key(&self.paths).ok();
        self.set_guard_judge(Some(Judge::new(reqwest::Client::new(), &base, key, options)));
        *self.judge_status.lock().unwrap() = JudgeStatus::Running;
        self.bus.emit(AppEvent::Judge);
    }

    async fn start_judge(&self, model: &InstalledModel) -> anyhow::Result<LlamaServer> {
        let binary = runtime::find_llama_server().context("llama-server was not found")?;
        let log_file = self.paths.logs().join("judge-server.log");
        let config = ServerConfig {
            binary,
            model_path: model.entry_path().context("judge model has no entry file")?,
            mmproj_path: None,
            alias: model.alias.clone(),
            context: catalog::JUDGE_CONTEXT,
            port: runtime::free_port()?,
            api_key_file: self.paths.upstream_api_key(),
            log_file: log_file.clone(),
            prompt_cache: 0,
            decision: true,
            sampling: None,
            speculative: Speculative::None,
            reasoning_budget: None,
            // The judge's threshold was set with a 16-bit cache.
            kv: KvType::F16,
            slot_dir: None,
        };
        let mut server = LlamaServer::start(config).await?;
        self.remember_pid("judge", server.pid());
        if let Err(e) = server.wait_ready(LOAD_TIMEOUT).await {
            self.remember_pid("judge", None);
            server.stop().await;
            return Err(e.context(format!("see {}", log_file.display())));
        }
        Ok(server)
    }

    fn set_guard_judge(&self, judge: Option<Judge>) {
        if let Some(guard) = self.guard.lock().unwrap().as_ref() {
            guard.set_judge(judge);
        }
    }

    pub fn save_settings(&self) -> anyhow::Result<()> {
        let text = serde_json::to_string_pretty(&*self.settings.lock().unwrap())?;
        superviser_core::paths::write_private(&self.paths.settings(), text.as_bytes())?;
        Ok(())
    }

    /// Generates a new local API key and updates the running guard server.
    pub fn regenerate_api_key(&self) -> anyhow::Result<String> {
        let key = secrets::regenerate_local_api_key(&self.paths)?;
        if let Some(guard) = self.guard.lock().unwrap().as_ref() {
            guard.set_api_key(Some(key.clone()));
        }
        Ok(key)
    }

    /// Strings that name the app's state directory, however a shell might spell it.
    fn state_markers(&self) -> Vec<String> {
        let mut markers = vec![
            "me.aagedal.AgentSuperviser".to_string(),
            self.paths.data.display().to_string(),
        ];
        if let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from)
            && let Ok(rel) = self.paths.data.strip_prefix(&home)
        {
            for prefix in ["~", "$HOME", "${HOME}"] {
                markers.push(format!("{prefix}/{}", rel.display()));
            }
        }
        markers
    }

    /// Starts the guard on the configured port (or any free one if it is taken).
    /// Guard call events are recorded in `self.events` and emitted on `self.bus`.
    pub async fn start_guard(self: &Arc<Self>, cors_origins: Vec<String>) -> anyhow::Result<()> {
        let port = self.settings.lock().unwrap().guard_port;
        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => listener,
            Err(e) => {
                tracing::warn!("guard port {port} is taken ({e}); using another");
                tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?
            }
        };
        let names: Vec<String> = PROTECTED_NAMES.iter().map(|s| s.to_string()).collect();
        let event_app = self.clone();
        let on_event: superviser_guard::guard::EventSink = Arc::new(move |event: &CallEvent| {
            event_app.record_event(event);
            event_app.bus.emit(AppEvent::GuardCall(event.clone()));
        });
        let config = superviser_guard::Config {
            block: self.settings.lock().unwrap().block,
            self_protection: Some(SelfProtection::new(&names, &self.state_markers())),
            journal: Some(Journal::new(self.paths.journal())),
            on_event: Some(on_event),
            ..Default::default()
        };
        let options = ProxyOptions {
            api_key: Some(secrets::local_api_key(&self.paths)?),
            upstream_api_key: Some(secrets::upstream_api_key(&self.paths)?),
            cors_origins,
            // Set once the judge's server runs (`sync_judge`).
            judge: None,
            // Agents run tools between model calls, so the Mac stays awake a while after each.
            on_model_call: Some(Arc::new(|| Box::new(AwakeAfter(Some(Awake::hold()))))),
        };
        let server = GuardServer::start(listener, config, options).await?;
        tracing::info!("guard listening on {}", server.addr);
        // Linux VMs reach the guard through this socket, never over the network.
        #[cfg(unix)]
        if let Err(e) = server.listen_unix(&self.paths.guard_socket()) {
            tracing::warn!("guard socket {}: {e}", self.paths.guard_socket().display());
        }
        *self.guard.lock().unwrap() = Some(server);
        self.sync_projects();
        Ok(())
    }

    /// Gives the guard every project's rules.
    pub fn sync_projects(&self) {
        let rules = self
            .settings
            .lock()
            .unwrap()
            .projects
            .iter()
            .map(|p| {
                let rules = ProjectRules {
                    name: p.name.clone(),
                    allowed_hosts: p.allowed_hosts.clone(),
                    blocked_tools: p.blocked_tools(),
                    blocked_programs: p.blocked_programs.clone(),
                };
                (p.id.clone(), rules)
            })
            .collect();
        if let Some(guard) = self.guard.lock().unwrap().as_ref() {
            guard.set_projects(rules);
        }
    }

    pub fn guard_url(&self) -> Option<String> {
        self.guard.lock().unwrap().as_ref().map(|g| format!("http://{}/v1", g.addr))
    }

    pub fn record_event(&self, event: &CallEvent) {
        let mut events = self.events.lock().unwrap();
        events.push_front(event.clone());
        events.truncate(MAX_EVENTS);
    }

    /// Starts downloading `model` on the current Tokio runtime and emits `AppEvent::Download` updates.
    pub fn start_download_model(self: &Arc<Self>, model: CatalogModel) -> anyhow::Result<String> {
        installed::validate_model(&model)?;
        if self.fit(&model).is_none() {
            bail!("This model is too large for this machine's memory.");
        }
        let id = installed::model_id(&model);
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut downloads = self.downloads.lock().unwrap();
            if downloads.contains_key(&id) {
                bail!("already downloading");
            }
            downloads.insert(id.clone(), cancel.clone());
        }
        let needs = self.fit(&model).map(|fit| fit.required);
        let name = model.name.clone();
        let initial = DownloadEvent {
            id: id.clone(),
            name: name.clone(),
            needs,
            done: 0,
            total: model.quant.size,
            finished: false,
            error: None,
        };
        self.active_downloads.lock().unwrap().insert(id.clone(), initial.clone());
        self.bus.emit(AppEvent::Download(initial));

        let app = self.clone();
        let task_id = id.clone();
        tokio::spawn(async move {
            let _awake = Awake::hold();
            let mut last = Instant::now();
            let result = installed::install(&Self::hub(), &app.paths, &model, cancel, |p| {
                if last.elapsed().as_millis() >= 250 || p.done == p.total {
                    last = Instant::now();
                    let ev = DownloadEvent {
                        id: task_id.clone(),
                        name: name.clone(),
                        needs,
                        done: p.done,
                        total: p.total,
                        finished: false,
                        error: None,
                    };
                    app.active_downloads.lock().unwrap().insert(task_id.clone(), ev.clone());
                    app.bus.emit(AppEvent::Download(ev));
                }
            })
            .await;
            app.downloads.lock().unwrap().remove(&task_id);
            app.active_downloads.lock().unwrap().remove(&task_id);
            let (finished, error) = match result {
                Ok(_) => (true, None),
                Err(e) => (false, Some(format!("{e:#}"))),
            };
            app.bus.emit(AppEvent::Download(DownloadEvent {
                id: task_id,
                name,
                needs,
                done: 0,
                total: 0,
                finished,
                error,
            }));
        });
        Ok(id)
    }

    /// Starts downloading the judge's model on the current Tokio runtime, then starts it if enabled.
    pub fn start_download_judge(self: &Arc<Self>) -> anyhow::Result<()> {
        let model = self.catalog.judge.clone().context("this version has no judge model")?;
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut downloads = self.downloads.lock().unwrap();
            if downloads.contains_key(JUDGE_DOWNLOAD) {
                bail!("already downloading");
            }
            downloads.insert(JUDGE_DOWNLOAD.into(), cancel.clone());
        }
        let initial = DownloadEvent {
            id: JUDGE_DOWNLOAD.into(),
            name: JUDGE_NAME.into(),
            needs: None,
            done: 0,
            total: model.quant.size,
            finished: false,
            error: None,
        };
        self.active_downloads.lock().unwrap().insert(JUDGE_DOWNLOAD.into(), initial.clone());
        self.bus.emit(AppEvent::Download(initial));

        let app = self.clone();
        tokio::spawn(async move {
            let _awake = Awake::hold();
            let mut last = Instant::now();
            let dir = installed::judge_dir(&app.paths, &model);
            let result = installed::install_to(&Self::hub(), &dir, &model, cancel, |p| {
                if last.elapsed().as_millis() >= 250 || p.done == p.total {
                    last = Instant::now();
                    let ev = DownloadEvent {
                        id: JUDGE_DOWNLOAD.into(),
                        name: JUDGE_NAME.into(),
                        needs: None,
                        done: p.done,
                        total: p.total,
                        finished: false,
                        error: None,
                    };
                    app.active_downloads.lock().unwrap().insert(JUDGE_DOWNLOAD.into(), ev.clone());
                    app.bus.emit(AppEvent::Download(ev));
                }
            })
            .await;
            app.downloads.lock().unwrap().remove(JUDGE_DOWNLOAD);
            app.active_downloads.lock().unwrap().remove(JUDGE_DOWNLOAD);
            let (finished, error) = match result {
                Ok(_) => (true, None),
                Err(e) => (false, Some(format!("{e:#}"))),
            };
            app.bus.emit(AppEvent::Download(DownloadEvent {
                id: JUDGE_DOWNLOAD.into(),
                name: JUDGE_NAME.into(),
                needs: None,
                done: 0,
                total: 0,
                finished,
                error,
            }));
            if finished {
                app.sync_judge().await;
            }
            app.bus.emit(AppEvent::Judge);
        });
        Ok(())
    }

    pub fn cancel_download(&self, id: &str) {
        if let Some(cancel) = self.downloads.lock().unwrap().get(id) {
            cancel.store(true, Ordering::Relaxed);
        }
    }

    pub fn active_downloads(&self) -> Vec<DownloadEvent> {
        self.active_downloads.lock().unwrap().values().cloned().collect()
    }

    /// The app's own models, then those found from other apps (an id the app has wins).
    pub fn installed(&self) -> Vec<InstalledModel> {
        let mut models = installed::list(&self.paths);
        for found in &self.discovery.lock().unwrap().models {
            if !models.iter().any(|m| m.id == found.id) {
                let mut found = found.clone();
                // A curated model keeps its curated name, whoever downloaded it.
                if let Some(curated) = self.catalog.models.iter().find(|c| c.repo.eq_ignore_ascii_case(&found.model.repo)) {
                    found.model.name = curated.name.clone();
                }
                models.push(found);
            }
        }
        models
    }

    pub fn find_installed(&self, id: &str) -> Option<InstalledModel> {
        self.installed().into_iter().find(|m| m.id == id)
    }

    /// Deletes one of the app's own models. Other apps' files are never touched.
    pub fn delete_model(&self, id: &str) -> anyhow::Result<()> {
        if self.models().iter().any(|m| m.id == id && !matches!(m.state, LoadState::Failed { .. })) {
            bail!("Unload the model before deleting it.");
        }
        let model = self.find_installed(id).with_context(|| format!("{id} is not installed"))?;
        if model.source != Source::App {
            bail!("{} belongs to {}; remove it there.", model.model.name, model.source.label());
        }
        self.conversations.forget(id);
        installed::remove(&self.paths, id)?;
        self.bus.emit(AppEvent::Discovery);
        Ok(())
    }

    pub fn fit(&self, model: &CatalogModel) -> Option<Fit> {
        model.fit(self.model_hardware().model_budget)
    }

    /// Every loaded, loading or failed model, most recently used first.
    pub fn models(&self) -> Vec<LoadedModel> {
        self.models.lock().unwrap().clone()
    }

    /// One line for the menu and simple checks: a running model if any (the most recently
    /// used), else one loading, else one that failed.
    pub fn status(&self) -> ServerStatus {
        let models = self.models.lock().unwrap();
        let pick = |want: fn(&LoadState) -> bool| models.iter().find(|m| want(&m.state));
        if let Some(m) = pick(|s| matches!(s, LoadState::Running { .. })) {
            let LoadState::Running { context } = m.state else { unreachable!() };
            return ServerStatus::Running { model: m.id.clone(), name: m.name.clone(), context };
        }
        if let Some(m) = pick(|s| matches!(s, LoadState::Loading { .. })) {
            let LoadState::Loading { progress, stage } = &m.state else { unreachable!() };
            return ServerStatus::Loading {
                model: m.id.clone(),
                name: m.name.clone(),
                progress: *progress,
                stage: stage.clone(),
            };
        }
        if let Some(m) = pick(|s| matches!(s, LoadState::Failed { .. })) {
            let LoadState::Failed { error } = &m.state else { unreachable!() };
            return ServerStatus::Failed { model: m.id.clone(), error: error.clone() };
        }
        ServerStatus::Stopped
    }

    /// A loaded model and its context.
    pub fn loaded(&self, id: &str) -> Option<(InstalledModel, u64)> {
        let context = self.models.lock().unwrap().iter().find_map(|m| match m.state {
            LoadState::Running { context } if m.id == id => Some(context),
            _ => None,
        })?;
        self.find_installed(id).map(|m| (m, context))
    }

    /// The most recently used loaded model.
    pub fn latest_model(&self) -> Option<(InstalledModel, u64)> {
        let id = self
            .models
            .lock()
            .unwrap()
            .iter()
            .find(|m| matches!(m.state, LoadState::Running { .. }))?
            .id
            .clone();
        self.loaded(&id)
    }

    /// Every loaded model, most recently used first.
    pub fn loaded_models(&self) -> Vec<(InstalledModel, u64)> {
        let ids: Vec<String> = self
            .models
            .lock()
            .unwrap()
            .iter()
            .filter(|m| matches!(m.state, LoadState::Running { .. }))
            .map(|m| m.id.clone())
            .collect();
        ids.iter().filter_map(|id| self.loaded(id)).collect()
    }

    /// Marks a model as just used, so it's the last one suggested for unloading.
    pub fn touch(&self, id: &str) {
        let mut models = self.models.lock().unwrap();
        if let Some(index) = models.iter().position(|m| m.id == id) {
            let model = models.remove(index);
            models.insert(0, model);
        }
    }

    /// Memory the loaded and loading models take besides `except`: (model budget, RAM).
    fn used(models: &[LoadedModel], except: &str) -> (u64, u64) {
        models
            .iter()
            .filter(|m| m.id != except && !matches!(m.state, LoadState::Failed { .. }))
            .fold((0, 0), |(gpu, ram), m| (gpu + m.memory, ram + m.ram))
    }

    /// How `model` fits beside the loaded models.
    fn fit_beside(&self, models: &[LoadedModel], model: &CatalogModel, id: &str) -> Option<Fit> {
        self.fit_beside_with(models, model, id, KvType::default())
    }

    fn fit_beside_with(&self, models: &[LoadedModel], model: &CatalogModel, id: &str, kv: KvType) -> Option<Fit> {
        let (used, _) = Self::used(models, id);
        model.fit_with(self.model_hardware().model_budget.checked_sub(used)?, kv)
    }

    /// Whether `id` can load now, else what to unload first.
    pub fn room_for(&self, id: &str) -> anyhow::Result<Room> {
        let model = self.find_installed(id).with_context(|| format!("{id} is not installed"))?;
        let mut models = self.models.lock().unwrap().clone();
        if models.iter().any(|m| m.id == id && !matches!(m.state, LoadState::Loading { .. } | LoadState::Failed { .. }))
            || self.fit_beside(&models, &model.model, id).is_some()
        {
            return Ok(Room { fits: true, unload: Vec::new() });
        }
        // Unload the least recently used until it fits.
        let mut unload = Vec::new();
        while let Some(index) = models.iter().rposition(|m| matches!(m.state, LoadState::Running { .. })) {
            unload.push(models.remove(index).id);
            if self.fit_beside(&models, &model.model, id).is_some() {
                return Ok(Room { fits: false, unload });
            }
        }
        let loading: Vec<String> = models
            .iter()
            .filter(|m| m.id != id && matches!(m.state, LoadState::Loading { .. }))
            .map(|m| m.name.clone())
            .collect();
        if !loading.is_empty() && self.fit(&model.model).is_some() {
            bail!("{} is still loading; wait for it or unload it first", loading.join(" and "));
        }
        bail!("{} doesn't fit in this machine's memory", model.model.name)
    }

    /// Safe unique file stem for a model id (avoids alias collisions in `.pid` and `.log` files).
    fn model_file_key(id: &str) -> String {
        let key: String = id
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
            .collect();
        if key.is_empty() || key == "." || key == ".." { "model".into() } else { key }
    }

    fn emit_status(&self) {
        self.bus.emit(AppEvent::Status(self.status()));
    }

    /// Loads `id` in its own llama-server beside the loaded models, if it fits, and points the
    /// guard (and OpenCode) at it. Emits `AppEvent::Status` on every state change.
    pub async fn load_model(&self, id: &str) -> anyhow::Result<()> {
        let model = self.find_installed(id).with_context(|| format!("{id} is not installed"))?;
        let binary = runtime::find_llama_server()
            .context("llama-server was not found (install llama.cpp, or set SUPERVISER_LLAMA_SERVER)")?;
        let entry = model.entry_path().context("model has no entry file")?;
        secrets::upstream_api_key(&self.paths)?;
        let load_id = self.next_load_id.fetch_add(1, Ordering::Relaxed);
        let file_key = Self::model_file_key(id);
        let curated = self.catalog.curated(&model.model.repo);
        let spec_n_max = curated.and_then(|m| m.spec_draft_n_max).unwrap_or(DEFAULT_SPEC_DRAFT_N_MAX);
        // Check the room and claim it in one go, so two loads can't both take it.
        let claimed = 'claim: {
            let mut models = self.models.lock().unwrap();
            if let Some(existing) = models.iter().find(|m| m.id == id) {
                match existing.state {
                    LoadState::Running { .. } => break 'claim None,
                    LoadState::Loading { .. } => bail!("{} is already loading", model.model.name),
                    LoadState::Failed { .. } => {}
                }
            }
            let fit = match self.fit_beside(&models, &model.model, id) {
                Some(fit) => fit,
                None => {
                    let others: Vec<&str> = models
                        .iter()
                        .filter(|m| m.id != id && !matches!(m.state, LoadState::Failed { .. }))
                        .map(|m| m.name.as_str())
                        .collect();
                    if others.is_empty() {
                        bail!("{} doesn't fit in this machine's memory", model.model.name);
                    }
                    bail!("{} doesn't fit beside {}; unload one first", model.model.name, others.join(" and "));
                }
            };
            let (gpu_used, ram_used) = Self::used(&models, id);
            let hw = self.model_hardware();
            let spare_gpu = hw.model_budget.saturating_sub(gpu_used.saturating_add(fit.required));
            let (speculative, draft_reserve) = runtime::detect_speculative(&entry, spare_gpu, spec_n_max);
            let model_memory = fit.required.saturating_add(draft_reserve);
            let prompt_cache = model
                .model
                .prompt_cache(&fit, hw.ram_budget.saturating_sub(ram_used.saturating_add(draft_reserve)));
            models.retain(|m| m.id != id);
            let state = LoadState::Loading {
                progress: Some(0.03),
                stage: Some("Starting llama-server…".into()),
            };
            models.insert(
                0,
                LoadedModel {
                    id: id.to_string(),
                    name: model.model.name.clone(),
                    alias: model.alias.clone(),
                    state,
                    memory: model_memory,
                    speculative: speculative.label(),
                    ram: model_memory.saturating_add(prompt_cache),
                    load_id,
                },
            );
            Some((fit, prompt_cache, speculative, draft_reserve))
        };
        let Some((mut fit, mut prompt_cache, initial_speculative, mut draft_reserve)) = claimed else {
            self.touch(id);
            self.sync_upstreams().await;
            self.emit_status();
            return Ok(());
        };
        self.emit_status();
        tracing::info!(
            "{}: {}k context, {} MiB for saved conversations, speculative: {}",
            model.alias,
            fit.context / 1024,
            prompt_cache >> 20,
            initial_speculative.label().as_deref().unwrap_or("off"),
        );
        let log_file = self.paths.logs().join(format!("llama-server-{file_key}.log"));
        let save_conversations = self.settings.lock().unwrap().save_conversations;
        let slot_dir = match save_conversations.then(|| self.conversations.prepare()) {
            Some(Ok(dir)) => Some(dir.to_path_buf()),
            Some(Err(e)) => {
                tracing::warn!("saved conversations: {e}");
                None
            }
            None => None,
        };
        let started = async {
            let mmproj_path = runtime::find_companion_mmproj(&entry);
            if let Some(mmproj) = &mmproj_path {
                tracing::info!("{}: using companion vision projector {}", model.alias, mmproj.display());
            }
            let mut config = ServerConfig {
                binary,
                model_path: entry.clone(),
                mmproj_path,
                alias: model.alias.clone(),
                context: fit.context,
                port: runtime::free_port()?,
                api_key_file: self.paths.upstream_api_key(),
                log_file: log_file.clone(),
                prompt_cache,
                decision: false,
                sampling: curated.and_then(|m| m.sampling),
                reasoning_budget: curated.and_then(|m| m.reasoning_budget),
                speculative: initial_speculative,
                kv: fit.kv,
                slot_dir: slot_dir.clone(),
            };
            loop {
                let mut server = LlamaServer::start(config.clone()).await?;
                if let Some(pid) = server.pid() {
                    self.loading_pids.lock().unwrap().insert(load_id, pid);
                    self.remember_pid(&file_key, Some(pid));
                }
                match server
                    .wait_ready_with_progress(LOAD_TIMEOUT, |p| {
                        if let Some(m) = self
                            .models
                            .lock()
                            .unwrap()
                            .iter_mut()
                            .find(|m| m.id == id && m.load_id == load_id)
                        {
                            m.state = LoadState::Loading {
                                progress: Some(p.fraction),
                                stage: Some(p.stage.clone()),
                            };
                        }
                        self.bus.emit(AppEvent::LoadProgress {
                            model: id.to_string(),
                            name: model.model.name.clone(),
                            progress: p.fraction,
                            stage: p.stage,
                        });
                    })
                    .await
                {
                    Ok(()) => {
                        let active_spec = config.speculative.label();
                        if let Some(m) = self
                            .models
                            .lock()
                            .unwrap()
                            .iter_mut()
                            .find(|m| m.id == id && m.load_id == load_id)
                        {
                            m.speculative = active_spec.clone();
                        }
                        if let Some(spec) = active_spec {
                            tracing::info!("{}: active speculative decoding: {spec}", model.alias);
                        }
                        break anyhow::Ok(server);
                    }
                    // Step down speculative decoding (`Draft+ngram` → `Draft` → `ngram` → `none`)
                    // if llama.cpp rejects the current strategy while loading.
                    Err(e) if !server.is_running() && config.speculative.downgrade().is_some() => {
                        let next_spec = config.speculative.downgrade().unwrap();
                        self.loading_pids.lock().unwrap().remove(&load_id);
                        self.remember_pid(&file_key, None);
                        tracing::warn!(
                            "{}: {e:#}; retrying speculative decoding as {}",
                            model.alias,
                            next_spec.label().as_deref().unwrap_or("off")
                        );
                        if !matches!(next_spec, Speculative::Draft { model_path: Some(_), .. }) && draft_reserve > 0 {
                            draft_reserve = 0;
                            if let Some(m) = self
                                .models
                                .lock()
                                .unwrap()
                                .iter_mut()
                                .find(|m| m.id == id && m.load_id == load_id)
                            {
                                m.memory = fit.required;
                                m.ram = fit.required.saturating_add(prompt_cache);
                            }
                        }
                        config.speculative = next_spec;
                        config.port = runtime::free_port()?;
                    }
                    // If a companion vision projector prevented loading, retry without `--mmproj`.
                    Err(e) if !server.is_running() && config.mmproj_path.is_some() => {
                        self.loading_pids.lock().unwrap().remove(&load_id);
                        self.remember_pid(&file_key, None);
                        tracing::warn!("{}: {e:#}; retrying without companion vision projector", model.alias);
                        config.mmproj_path = None;
                        config.port = runtime::free_port()?;
                    }
                    // Nor one whose attention can't use an 8-bit cache: with a 16-bit one, if it fits.
                    Err(e) if config.kv != KvType::F16 && !server.is_running() => {
                        self.loading_pids.lock().unwrap().remove(&load_id);
                        self.remember_pid(&file_key, None);
                        let mut models = self.models.lock().unwrap();
                        let Some(f16) = self.fit_beside_with(&models, &model.model, id, KvType::F16) else {
                            break Err(e.context(format!(
                                "{} needs a 16-bit context cache here, which doesn't fit beside the loaded models",
                                model.model.name
                            )));
                        };
                        tracing::warn!(
                            "{}: {e:#}; loading it again with a 16-bit cache at {}k context",
                            model.alias,
                            f16.context / 1024
                        );
                        let (gpu_used, ram_used) = Self::used(&models, id);
                        let hw = self.model_hardware();
                        fit = f16;
                        let spare_gpu = hw.model_budget.saturating_sub(gpu_used.saturating_add(fit.required));
                        let (speculative, new_draft_reserve) =
                            runtime::detect_speculative(&entry, spare_gpu, spec_n_max);
                        draft_reserve = new_draft_reserve;
                        let model_memory = fit.required.saturating_add(draft_reserve);
                        prompt_cache = model
                            .model
                            .prompt_cache(&fit, hw.ram_budget.saturating_sub(ram_used.saturating_add(draft_reserve)));
                        if let Some(m) = models.iter_mut().find(|m| m.id == id && m.load_id == load_id) {
                            (m.memory, m.ram, m.speculative) = (
                                model_memory,
                                model_memory.saturating_add(prompt_cache),
                                speculative.label(),
                            );
                        }
                        drop(models);
                        (config.kv, config.context, config.prompt_cache, config.speculative) =
                            (fit.kv, fit.context, prompt_cache, speculative);
                        config.port = runtime::free_port()?;
                    }
                    Err(e) => {
                        self.loading_pids.lock().unwrap().remove(&load_id);
                        self.remember_pid(&file_key, None);
                        server.stop().await;
                        break Err(e.context(format!("see {}", log_file.display())));
                    }
                }
            }
        }
        .await;
        let still_active = || self.models.lock().unwrap().iter().any(|m| m.id == id && m.load_id == load_id);
        let set_state = |state: LoadState| {
            if let Some(m) = self.models.lock().unwrap().iter_mut().find(|m| m.id == id && m.load_id == load_id) {
                m.state = state;
            }
        };
        match started {
            Ok(mut server) => {
                // Unloaded or superseded while loading: don't restore or keep it.
                if !still_active() {
                    self.loading_pids.lock().unwrap().remove(&load_id);
                    self.remember_pid(&file_key, None);
                    server.stop().await;
                    self.emit_status();
                    bail!("{} was unloaded while it loaded", model.model.name);
                }
                let key = slot_dir
                    .as_ref()
                    .and_then(|_| conversations::Key::of(&model.entry_path()?, fit.kv, fit.context));
                if let Some(key) = &key {
                    let stage = "Restoring saved conversation…".to_string();
                    set_state(LoadState::Loading {
                        progress: Some(0.96),
                        stage: Some(stage.clone()),
                    });
                    self.bus.emit(AppEvent::LoadProgress {
                        model: id.to_string(),
                        name: model.model.name.clone(),
                        progress: 0.96,
                        stage,
                    });
                    if let Some(tokens) = self.conversations.restore(&server, id, key).await {
                        tracing::info!("{}: picked up its last conversation ({tokens} tokens)", model.alias);
                    }
                }
                // Re-check under the `running` lock so an unload during `restore()` can never
                // leave an orphaned server invisible to `self.models`.
                {
                    let mut running = self.running.lock().await;
                    if !still_active() {
                        drop(running);
                        self.loading_pids.lock().unwrap().remove(&load_id);
                        self.remember_pid(&file_key, None);
                        server.stop().await;
                        self.emit_status();
                        bail!("{} was unloaded while it loaded", model.model.name);
                    }
                    running.insert(id.to_string(), Running { server, model: model.model.clone(), key });
                    self.loading_pids.lock().unwrap().remove(&load_id);
                    set_state(LoadState::Running { context: fit.context });
                }
                self.sync_upstreams().await;
                self.emit_status();
                self.settings.lock().unwrap().last_model = Some(id.to_string());
                self.save_settings()?;
                self.sync_judge().await;
                if self.settings.lock().unwrap().configure_opencode
                    && let Err(e) = self.setup_opencode()
                {
                    tracing::warn!("OpenCode setup: {e:#}");
                }
                Ok(())
            }
            Err(e) => {
                self.loading_pids.lock().unwrap().remove(&load_id);
                set_state(LoadState::Failed { error: format!("{e:#}") });
                self.emit_status();
                Err(e)
            }
        }
    }

    /// Unloads `id` (or, with `None`, every model), freeing its memory.
    pub async fn unload_model(&self, id: Option<&str>) {
        let stopping: Vec<(String, Running)> = {
            let mut running = self.running.lock().await;
            let ids: Vec<String> = running
                .keys()
                .filter(|k| id.is_none_or(|id| id == k.as_str()))
                .cloned()
                .collect();
            ids.into_iter().filter_map(|k| running.remove(&k).map(|r| (k, r))).collect()
        };
        // Kill any mid-load processes matching the unloaded model(s) too.
        let cancel_load_ids: Vec<u64> = self
            .models
            .lock()
            .unwrap()
            .iter()
            .filter(|m| id.is_none_or(|id| id == m.id))
            .map(|m| m.load_id)
            .collect();
        for load_id in cancel_load_ids {
            if let Some(pid) = self.loading_pids.lock().unwrap().remove(&load_id) {
                let _ = std::process::Command::new("/bin/kill").arg(pid.to_string()).status();
            }
        }
        self.models.lock().unwrap().retain(|m| id.is_some_and(|id| id != m.id));
        for (model_id, mut running) in stopping {
            self.save_conversation(&model_id, &running).await;
            self.remember_pid(&Self::model_file_key(&model_id), None);
            running.server.stop().await;
        }
        self.sync_upstreams().await;
        self.sync_judge().await;
        self.emit_status();
    }

    /// Saves a model's conversation before it stops, if conversations are saved.
    async fn save_conversation(&self, id: &str, running: &Running) {
        let Some(key) = &running.key else { return };
        if !self.settings.lock().unwrap().save_conversations {
            return;
        }
        match self
            .conversations
            .save(&running.server, id, key, |tokens| running.model.saved_bytes(tokens, key.kv))
            .await
        {
            Ok(Some(saved)) => tracing::info!(
                "{}: saved its conversation ({} tokens, {} MB)",
                running.server.alias,
                saved.tokens,
                saved.bytes / 1_000_000
            ),
            Ok(None) => {}
            Err(e) => tracing::warn!("{}: saving its conversation: {e:#}", running.server.alias),
        }
    }

    /// Turns saving conversations on or off; off deletes the saved ones.
    pub fn set_save_conversations(&self, enabled: bool) -> anyhow::Result<()> {
        self.settings.lock().unwrap().save_conversations = enabled;
        self.save_settings()?;
        if !enabled {
            self.conversations.clear();
        }
        Ok(())
    }

    /// Toggles sharing local file paths alongside dragged & dropped images.
    pub fn set_include_image_paths(&self, enabled: bool) -> anyhow::Result<()> {
        self.settings.lock().unwrap().include_image_paths = enabled;
        self.save_settings()
    }

    /// Records (or with `None` forgets) a model server's pid in `run/`, for [`runtime::stop_leftovers`].
    fn remember_pid(&self, name: &str, pid: Option<u32>) {
        let file = self.paths.run().join(format!("{name}.pid"));
        match pid {
            Some(pid) => {
                let _ = superviser_core::paths::private_dir(&self.paths.run());
                let _ = superviser_core::paths::write_private(&file, pid.to_string().as_bytes());
            }
            None => {
                let _ = std::fs::remove_file(file);
            }
        }
    }

    /// Points the guard at every loaded model's server, by alias; requests naming none of
    /// them go to the most recently loaded. Iterates LRU→MRU so a newer model wins alias collisions.
    pub async fn sync_upstreams(&self) {
        let urls: HashMap<String, String> = self
            .running
            .lock()
            .await
            .iter()
            .map(|(id, r)| (id.clone(), r.server.base_url()))
            .collect();
        let models = self.models();
        let by_alias: HashMap<String, String> = models
            .iter()
            .rev()
            .filter_map(|m| Some((m.alias.clone(), urls.get(&m.id)?.clone())))
            .collect();
        let default = models.iter().find_map(|m| urls.get(&m.id).cloned());
        if let Some(guard) = self.guard.lock().unwrap().as_ref() {
            guard.set_upstreams(by_alias, default);
        }
        self.sync_reasoning();
    }

    /// Remembered reasoning level for an installed model (`Default` if unset).
    pub fn model_reasoning(&self, id: &str) -> ReasoningLevel {
        self.settings
            .lock()
            .unwrap()
            .model_reasoning
            .get(id)
            .copied()
            .unwrap_or_default()
    }

    /// Sets and persists the reasoning level for `id`, updating the live guard immediately.
    pub fn set_model_reasoning(&self, id: &str, level: ReasoningLevel) -> anyhow::Result<()> {
        {
            let mut settings = self.settings.lock().unwrap();
            if level == ReasoningLevel::Default {
                settings.model_reasoning.remove(id);
            } else {
                settings.model_reasoning.insert(id.to_string(), level);
            }
        }
        self.save_settings()?;
        self.sync_reasoning();
        self.bus.emit(AppEvent::Reasoning);
        Ok(())
    }

    /// Pushes each loaded model's remembered reasoning level into the running guard proxy.
    pub fn sync_reasoning(&self) {
        let models = self.models();
        let levels = self.settings.lock().unwrap().model_reasoning.clone();
        let by_alias: HashMap<String, String> = models
            .iter()
            .rev()
            .filter(|m| matches!(m.state, LoadState::Running { .. }))
            .filter_map(|m| {
                let api = levels.get(&m.id).copied().unwrap_or_default().api_value()?;
                Some((m.alias.clone(), api.to_string()))
            })
            .collect();
        let default = models
            .iter()
            .find(|m| matches!(m.state, LoadState::Running { .. }))
            .and_then(|m| levels.get(&m.id).copied().unwrap_or_default().api_value())
            .map(str::to_string);
        if let Some(guard) = self.guard.lock().unwrap().as_ref() {
            guard.set_reasoning(by_alias, default);
        }
    }

    /// Adds the loaded models to OpenCode's global config, the most recently used as its default.
    pub fn setup_opencode(&self) -> anyhow::Result<opencode::Installed> {
        let loaded = self.loaded_models();
        let Some((latest, _)) = loaded.first() else { bail!("load a model first") };
        let base_url = self.guard_url().context("the guard is not running")?;
        secrets::local_api_key(&self.paths)?;
        let setup = opencode::Setup {
            base_url,
            api_key_file: self.paths.local_api_key(),
            models: loaded
                .iter()
                .map(|(m, context)| opencode::SetupModel {
                    alias: m.alias.clone(),
                    name: m.model.name.clone(),
                    context: *context,
                })
                .collect(),
            default_model: Some(latest.alias.clone()),
        };
        opencode::install(&setup)
    }

    /// The installed model a chat uses, if one was chosen for it.
    pub fn chat_model(&self, project: &str, chat: &str) -> Option<String> {
        self.chat_models.lock().unwrap().get(&format!("{project}/{chat}")).cloned()
    }

    /// Sets (or with `None` forgets) the model a chat uses.
    pub fn set_chat_model(&self, project: &str, chat: &str, model: Option<String>) -> anyhow::Result<()> {
        let mut models = self.chat_models.lock().unwrap();
        let key = format!("{project}/{chat}");
        match model {
            Some(model) => models.insert(key, model),
            None => models.remove(&key),
        };
        superviser_core::paths::write_private(&self.paths.chat_models(), serde_json::to_string(&*models)?.as_bytes())?;
        Ok(())
    }

    /// A project's archived chats.
    pub fn archived_chats(&self, project: &str) -> Vec<String> {
        let prefix = format!("{project}/");
        self.archived_chats
            .lock()
            .unwrap()
            .iter()
            .filter_map(|key| key.strip_prefix(&prefix).map(str::to_string))
            .collect()
    }

    /// Archives a chat, or puts it back in the list.
    pub fn set_chat_archived(&self, project: &str, chat: &str, archived: bool) -> anyhow::Result<()> {
        let mut chats = self.archived_chats.lock().unwrap();
        let key = format!("{project}/{chat}");
        let changed = if archived { chats.insert(key) } else { chats.remove(&key) };
        if changed {
            superviser_core::paths::write_private(
                &self.paths.archived_chats(),
                serde_json::to_string(&*chats)?.as_bytes(),
            )?;
        }
        Ok(())
    }

    /// Forgets what the app keeps about a deleted chat.
    pub fn forget_chat(&self, project: &str, chat: &str) -> anyhow::Result<()> {
        self.set_chat_model(project, chat, None)?;
        self.set_chat_archived(project, chat, false)
    }

    pub fn project(&self, id: &str) -> Option<Project> {
        self.settings.lock().unwrap().projects.iter().find(|p| p.id == id).cloned()
    }

    /// Adds `project`, or replaces the one with its id, and gives the guard its rules.
    pub fn save_project(&self, project: Project) -> anyhow::Result<Project> {
        let project = project.normalized()?;
        let backend = sandbox::backend(project.isolation);
        if let Err(why) = backend.available().and_then(|()| backend.suits(project.main())) {
            bail!("{why}");
        }
        {
            let mut settings = self.settings.lock().unwrap();
            match settings.projects.iter_mut().find(|p| p.id == project.id) {
                Some(existing) => *existing = project.clone(),
                None => settings.projects.insert(0, project.clone()),
            }
        }
        self.save_settings()?;
        self.sync_projects();
        self.bus.emit(AppEvent::Projects);
        Ok(project)
    }

    /// Setup guides & copyable config snippets for external harnesses (Pi, OpenCode, Codex, etc.).
    pub fn connect_guides(&self) -> Vec<superviser_core::connect::HarnessGuide> {
        let base_url = self.guard_url().unwrap_or_default();
        let api_key = secrets::local_api_key(&self.paths).unwrap_or_default();
        let latest = self.latest_model();
        let (alias, name, ctx) = match latest {
            Some((m, ctx)) => (m.alias, m.model.name, ctx),
            None => (String::new(), String::new(), superviser_core::catalog::MIN_CONTEXT),
        };
        superviser_core::connect::guides(&superviser_core::connect::ConnectParams {
            base_url: &base_url,
            api_key: &api_key,
            model_alias: &alias,
            model_name: &name,
            context_window: ctx,
        })
    }

    pub fn delete_project(&self, id: &str) -> anyhow::Result<()> {
        self.engines.stop(id);
        self.chats.stop_project(id);
        self.settings.lock().unwrap().projects.retain(|p| p.id != id);
        entropy::Chat::delete_all_in(&self.paths.data, id);
        {
            let mut chat_models = self.chat_models.lock().unwrap();
            let before = chat_models.len();
            chat_models.retain(|key, _| !key.starts_with(&format!("{id}/")));
            if chat_models.len() != before {
                superviser_core::paths::write_private(
                    &self.paths.chat_models(),
                    serde_json::to_string(&*chat_models)?.as_bytes(),
                )?;
            }
        }
        for chat in self.archived_chats(id) {
            self.set_chat_archived(id, &chat, false)?;
        }
        self.save_settings()?;
        self.sync_projects();
        self.bus.emit(AppEvent::Projects);
        Ok(())
    }

    /// The project whose main folder is `folder`, made if there is none.
    pub fn project_for_folder(&self, folder: &std::path::Path) -> anyhow::Result<Project> {
        let existing = self.settings.lock().unwrap().projects.iter().find(|p| p.main() == folder).cloned();
        match existing {
            Some(project) => Ok(project),
            None => self.save_project(Project::new(folder.to_path_buf())),
        }
    }

    /// Opens a project in OpenCode, in a new Terminal window, isolated and limited as it says.
    /// OpenCode's config is refreshed first so it talks to the guard about the running model.
    pub fn open_project(&self, id: &str) -> anyhow::Result<sandbox::Launched> {
        let project = self.project(id).context("that project no longer exists")?;
        if self.latest_model().is_none() {
            bail!("Load a model first: OpenCode would have nothing to talk to.");
        }
        self.setup_opencode()?;
        let guard_url = self.guard_url().context("the guard is not running")?;
        let model = self.latest_model().map(|(m, context)| opencode::SetupModel {
            alias: m.alias,
            name: m.model.name,
            context,
        });
        let launch = sandbox::Launch {
            project: &project,
            model,
            guard_url: guard_url.trim_end_matches("/v1").to_string(),
            opencode: project.opencode_config(&opencode::mcp_servers()),
        };
        let launched = sandbox::launch(&self.paths, &launch)?;
        // Most recently opened first.
        {
            let mut settings = self.settings.lock().unwrap();
            if let Some(index) = settings.projects.iter().position(|p| p.id == id) {
                let project = settings.projects.remove(index);
                settings.projects.insert(0, project);
            }
        }
        self.save_settings()?;
        self.bus.emit(AppEvent::Projects);
        Ok(launched)
    }

    /// Async shutdown: saves conversations, stops model servers, judge, engines, chats, and guard.
    pub async fn shutdown_async(&self) {
        self.chats.stop_all();
        self.engines.stop_all();
        // Stop any llama-server processes still mid-load before waiting on locks.
        for (_, pid) in self.loading_pids.lock().unwrap().drain() {
            let _ = std::process::Command::new("/bin/kill").arg(pid.to_string()).status();
        }
        if let Ok(mut running) = tokio::time::timeout(Duration::from_secs(5), self.running.lock()).await {
            for (id, mut running) in running.drain() {
                self.save_conversation(&id, &running).await;
                self.remember_pid(&Self::model_file_key(&id), None);
                running.server.stop().await;
            }
        }
        if let Ok(mut judge) = tokio::time::timeout(Duration::from_secs(2), self.judge_server.lock()).await
            && let Some(mut judge) = judge.take()
        {
            self.remember_pid("judge", None);
            judge.stop().await;
        }
        // Catch any server whose lock timed out (e.g. mid-`start_judge`).
        runtime::stop_leftovers(&self.paths.run(), &self.paths.upstream_api_key().display().to_string());
        if let Some(guard) = self.guard.lock().unwrap().as_mut() {
            guard.stop();
        }
    }

    /// Synchronous shutdown helper for UI quit callbacks.
    pub fn shutdown(&self, handle: &tokio::runtime::Handle) {
        handle.block_on(self.shutdown_async());
    }
}
