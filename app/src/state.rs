//! The app's state and what it can do, independent of windows and menus.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use superviser_core::awake::Awake;
use superviser_core::catalog::{self, Catalog, CatalogModel, Fit};
use superviser_core::discover::{self, Discovery, Locations};
use superviser_core::hardware::Hardware;
use superviser_core::installed::{self, InstalledModel, Source};
use superviser_core::paths::{PROTECTED_NAMES, Paths};
use superviser_core::project::Project;
use superviser_core::runtime::{self, LlamaServer, ServerConfig};
use superviser_core::sandbox::{self, Isolation, Network};
use superviser_core::{opencode, secrets};
use superviser_guard::journal::Journal;
use superviser_guard::selfprotect::SelfProtection;
use superviser_guard::{CallEvent, GuardServer, Judge, JudgeOptions, ProjectRules, ProxyOptions};

pub const DEFAULT_GUARD_PORT: u16 = 8787;
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
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(AWAKE_AFTER_CALL).await;
            drop(awake);
        });
    }
}
/// Origins of the app's own webviews (macOS/Linux, Windows).
const WEBVIEW_ORIGINS: &[&str] = &["tauri://localhost", "http://tauri.localhost", "https://tauri.localhost"];

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
            configure_opencode: true,
            use_other_apps: true,
            last_model: None,
            projects: Vec::new(),
            setup_done: false,
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
    Loading { model: String, name: String },
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
    /// Of system memory, with the conversations it keeps.
    #[serde(skip)]
    ram: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum LoadState {
    Loading,
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
    /// The model servers, by installed model id.
    running: tokio::sync::Mutex<HashMap<String, Running>>,
    /// Loaded, loading and failed models, most recently used first.
    models: Mutex<Vec<LoadedModel>>,
    judge_server: tokio::sync::Mutex<Option<LlamaServer>>,
    pub judge_status: Mutex<JudgeStatus>,
    pub downloads: Mutex<HashMap<String, Arc<AtomicBool>>>,
    pub events: Mutex<VecDeque<CallEvent>>,
    /// Models other apps downloaded, from the last scan.
    pub discovery: Mutex<Discovery>,
    /// The built-in agent's turns in progress.
    pub chats: crate::agent::Chats,
    /// OpenCode, run headless for the chat window, by project.
    pub engines: crate::acp::Engines,
    /// The model each chat uses, by `project/chat`: switching a chat's model makes the next
    /// reply read the whole chat again, so each keeps its own.
    chat_models: Mutex<HashMap<String, String>>,
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
        Ok(Self {
            hardware: Hardware::detect(),
            catalog: Catalog::bundled(),
            settings: Mutex::new(settings),
            guard: Mutex::new(None),
            running: tokio::sync::Mutex::new(HashMap::new()),
            models: Mutex::new(Vec::new()),
            judge_server: tokio::sync::Mutex::new(None),
            judge_status: Mutex::new(JudgeStatus::Idle),
            downloads: Mutex::new(HashMap::new()),
            events: Mutex::new(VecDeque::new()),
            discovery: Mutex::new(Discovery::default()),
            chats: crate::agent::Chats::default(),
            engines: crate::acp::Engines::default(),
            chat_models: Mutex::new(std::fs::read_to_string(paths.chat_models()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()),
            paths,
        })
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

    /// The memory budgets chat models get: what the judge leaves when it is on.
    pub fn model_hardware(&self) -> Hardware {
        match &self.catalog.judge {
            Some(judge) if self.settings.lock().unwrap().judge => self.hardware.less(judge.judge_required()),
            _ => self.hardware.clone(),
        }
    }

    /// Whether the judge fits beside at least one curated model at its smallest context.
    pub fn judge_fits(&self) -> bool {
        self.catalog.judge.as_ref().is_some_and(|judge| !self.catalog.offers(&self.hardware.less(judge.judge_required())).is_empty())
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
        Ok(())
    }

    /// Runs the judge while it is on, downloaded and a model is running; stops it otherwise.
    /// Options changes reach the guard without a restart.
    pub async fn sync_judge(&self) {
        let (enabled, fail_closed) = {
            let settings = self.settings.lock().unwrap();
            (settings.judge, settings.judge_fail_closed)
        };
        let wanted = if enabled && self.latest_model().is_some() { self.judge_installed() } else { None };
        let mut server = self.judge_server.lock().await;
        let Some(model) = wanted else {
            if let Some(mut old) = server.take() {
                self.remember_pid("judge", None);
                old.stop().await;
            }
            self.set_guard_judge(None);
            *self.judge_status.lock().unwrap() = JudgeStatus::Idle;
            return;
        };
        if server.as_mut().is_none_or(|s| !s.is_running()) {
            *self.judge_status.lock().unwrap() = JudgeStatus::Loading;
            match self.start_judge(&model).await {
                Ok(started) => {
                    self.remember_pid("judge", started.pid());
                    *server = Some(started);
                }
                Err(e) => {
                    tracing::warn!("judge: {e:#}");
                    *server = None;
                    self.set_guard_judge(None);
                    *self.judge_status.lock().unwrap() = JudgeStatus::Failed { error: format!("{e:#}") };
                    return;
                }
            }
        }
        let options = JudgeOptions { fail_closed, ..JudgeOptions::default() };
        let base = server.as_ref().unwrap().base_url();
        let key = secrets::upstream_api_key(&self.paths).ok();
        self.set_guard_judge(Some(Judge::new(reqwest::Client::new(), &base, key, options)));
        *self.judge_status.lock().unwrap() = JudgeStatus::Running;
    }

    async fn start_judge(&self, model: &InstalledModel) -> anyhow::Result<LlamaServer> {
        let binary = runtime::find_llama_server().context("llama-server was not found")?;
        let log_file = self.paths.logs().join("judge-server.log");
        let config = ServerConfig {
            binary,
            model_path: model.entry_path().context("judge model has no entry file")?,
            alias: model.alias.clone(),
            context: catalog::JUDGE_CONTEXT,
            port: runtime::free_port()?,
            api_key_file: self.paths.upstream_api_key(),
            log_file: log_file.clone(),
            prompt_cache: 0,
            decision: true,
            sampling: None,
            speculative: false,
            reasoning_budget: None,
        };
        let mut server = LlamaServer::start(config).await?;
        if let Err(e) = server.wait_ready(LOAD_TIMEOUT).await {
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

    /// Strings that name the app's state directory, however a shell might spell it.
    fn state_markers(&self) -> Vec<String> {
        let mut markers = vec!["me.aagedal.AgentSuperviser".to_string(), self.paths.data.display().to_string()];
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
    pub async fn start_guard(&self, on_event: superviser_guard::guard::EventSink) -> anyhow::Result<()> {
        let port = self.settings.lock().unwrap().guard_port;
        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => listener,
            Err(e) => {
                tracing::warn!("guard port {port} is taken ({e}); using another");
                tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?
            }
        };
        let names: Vec<String> = PROTECTED_NAMES.iter().map(|s| s.to_string()).collect();
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
            cors_origins: WEBVIEW_ORIGINS.iter().map(|s| s.to_string()).collect(),
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
                let rules = ProjectRules { name: p.name.clone(), allowed_hosts: p.allowed_hosts.clone(), blocked_tools: p.blocked_tools(), blocked_programs: p.blocked_programs.clone() };
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
        let model = self.find_installed(id).with_context(|| format!("{id} is not installed"))?;
        if model.source != Source::App {
            bail!("{} belongs to {}; remove it there.", model.model.name, model.source.label());
        }
        installed::remove(&self.paths, id)
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
        if let Some(m) = pick(|s| *s == LoadState::Loading) {
            return ServerStatus::Loading { model: m.id.clone(), name: m.name.clone() };
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
        let id = self.models.lock().unwrap().iter().find(|m| matches!(m.state, LoadState::Running { .. }))?.id.clone();
        self.loaded(&id)
    }

    /// Every loaded model, most recently used first.
    pub fn loaded_models(&self) -> Vec<(InstalledModel, u64)> {
        let ids: Vec<String> = self.models.lock().unwrap().iter().filter(|m| matches!(m.state, LoadState::Running { .. })).map(|m| m.id.clone()).collect();
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
        models.iter().filter(|m| m.id != except && !matches!(m.state, LoadState::Failed { .. })).fold((0, 0), |(gpu, ram), m| (gpu + m.memory, ram + m.ram))
    }

    /// How `model` fits beside the loaded models.
    fn fit_beside(&self, models: &[LoadedModel], model: &CatalogModel, id: &str) -> Option<Fit> {
        let (used, _) = Self::used(models, id);
        model.fit(self.model_hardware().model_budget.checked_sub(used)?)
    }

    /// Whether `id` can load now, else what to unload first.
    pub fn room_for(&self, id: &str) -> anyhow::Result<Room> {
        let model = self.find_installed(id).with_context(|| format!("{id} is not installed"))?;
        let mut models = self.models.lock().unwrap().clone();
        if models.iter().any(|m| m.id == id && m.state != LoadState::Loading && !matches!(m.state, LoadState::Failed { .. })) || self.fit_beside(&models, &model.model, id).is_some() {
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
        bail!("{} doesn't fit in this machine's memory", model.model.name)
    }

    /// Loads `id` in its own llama-server beside the loaded models, if it fits, and points the
    /// guard (and OpenCode) at it. `changed` is told when its state changes.
    pub async fn load_model(&self, id: &str, mut changed: impl FnMut()) -> anyhow::Result<()> {
        let model = self.find_installed(id).with_context(|| format!("{id} is not installed"))?;
        let binary = runtime::find_llama_server().context("llama-server was not found (install llama.cpp, or set SUPERVISER_LLAMA_SERVER)")?;
        let entry = model.entry_path().context("model has no entry file")?;
        secrets::upstream_api_key(&self.paths)?;
        // Check the room and claim it in one go, so two loads can't both take it.
        let (fit, prompt_cache) = {
            let mut models = self.models.lock().unwrap();
            if let Some(existing) = models.iter().find(|m| m.id == id) {
                match existing.state {
                    LoadState::Running { .. } => {
                        drop(models);
                        self.touch(id);
                        return Ok(());
                    }
                    LoadState::Loading => bail!("{} is already loading", model.model.name),
                    LoadState::Failed { .. } => {}
                }
            }
            let fit = match self.fit_beside(&models, &model.model, id) {
                Some(fit) => fit,
                None => {
                    let others: Vec<&str> = models.iter().filter(|m| m.id != id && matches!(m.state, LoadState::Running { .. })).map(|m| m.name.as_str()).collect();
                    bail!("{} doesn't fit beside {}; unload one first", model.model.name, others.join(" and "));
                }
            };
            let (_, ram_used) = Self::used(&models, id);
            let prompt_cache = model.model.prompt_cache(&fit, self.model_hardware().ram_budget.saturating_sub(ram_used));
            models.retain(|m| m.id != id);
            let state = LoadState::Loading;
            models.insert(0, LoadedModel { id: id.to_string(), name: model.model.name.clone(), alias: model.alias.clone(), state, memory: fit.required, ram: fit.required + prompt_cache });
            (fit, prompt_cache)
        };
        changed();
        tracing::info!("{}: {}k context, {} MiB for saved conversations", model.alias, fit.context / 1024, prompt_cache >> 20);
        let log_file = self.paths.logs().join(format!("llama-server-{}.log", model.alias));
        let curated = self.catalog.curated(&model.model.repo);
        let started = async {
            let mut config = ServerConfig {
                binary,
                model_path: entry,
                alias: model.alias.clone(),
                context: fit.context,
                port: runtime::free_port()?,
                api_key_file: self.paths.upstream_api_key(),
                log_file: log_file.clone(),
                prompt_cache,
                decision: false,
                sampling: curated.and_then(|m| m.sampling),
                reasoning_budget: curated.and_then(|m| m.reasoning_budget),
                speculative: true,
            };
            loop {
                let mut server = LlamaServer::start(config.clone()).await?;
                match server.wait_ready(LOAD_TIMEOUT).await {
                    Ok(()) => break anyhow::Ok(server),
                    // A model llama.cpp can't draft for may stop while loading: once more without.
                    Err(e) if config.speculative && !server.is_running() => {
                        tracing::warn!("{}: {e:#}; loading it again without speculative decoding", model.alias);
                        config.speculative = false;
                        config.port = runtime::free_port()?;
                    }
                    Err(e) => {
                        server.stop().await;
                        break Err(e.context(format!("see {}", log_file.display())));
                    }
                }
            }
        }
        .await;
        let set_state = |state: LoadState| {
            if let Some(m) = self.models.lock().unwrap().iter_mut().find(|m| m.id == id) {
                m.state = state;
            }
        };
        match started {
            Ok(server) => {
                // Unloaded while it loaded: don't keep it.
                if !self.models.lock().unwrap().iter().any(|m| m.id == id) {
                    let mut server = server;
                    server.stop().await;
                    bail!("{} was unloaded while it loaded", model.model.name);
                }
                self.remember_pid(&model.alias, server.pid());
                self.running.lock().await.insert(id.to_string(), Running { server });
                set_state(LoadState::Running { context: fit.context });
                self.sync_upstreams().await;
                changed();
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
                set_state(LoadState::Failed { error: format!("{e:#}") });
                changed();
                Err(e)
            }
        }
    }

    /// Unloads `id` (or, with `None`, every model), freeing its memory.
    pub async fn unload_model(&self, id: Option<&str>) {
        let stopping: Vec<Running> = {
            let mut running = self.running.lock().await;
            let ids: Vec<String> = running.keys().filter(|k| id.is_none_or(|id| id == k.as_str())).cloned().collect();
            ids.iter().filter_map(|k| running.remove(k)).collect()
        };
        self.models.lock().unwrap().retain(|m| id.is_some_and(|id| id != m.id));
        for mut running in stopping {
            self.remember_pid(&running.server.alias, None);
            running.server.stop().await;
        }
        self.sync_upstreams().await;
        self.sync_judge().await;
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
    /// them go to the most recently loaded.
    async fn sync_upstreams(&self) {
        let urls: HashMap<String, String> = self.running.lock().await.iter().map(|(id, r)| (id.clone(), r.server.base_url())).collect();
        let models = self.models();
        let by_alias: HashMap<String, String> = models.iter().filter_map(|m| Some((m.alias.clone(), urls.get(&m.id)?.clone()))).collect();
        let default = models.iter().find_map(|m| urls.get(&m.id).cloned());
        if let Some(guard) = self.guard.lock().unwrap().as_ref() {
            guard.set_upstreams(by_alias, default);
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
            models: loaded.iter().map(|(m, context)| opencode::SetupModel { alias: m.alias.clone(), name: m.model.name.clone(), context: *context }).collect(),
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

    pub fn project(&self, id: &str) -> Option<Project> {
        self.settings.lock().unwrap().projects.iter().find(|p| p.id == id).cloned()
    }

    /// Adds `project`, or replaces the one with its id, and gives the guard its rules.
    pub fn save_project(&self, project: Project) -> anyhow::Result<Project> {
        let project = project.normalized()?;
        let choice = sandbox::choices(project.main()).into_iter().find(|c| c.isolation == project.isolation);
        if let Some(why) = choice.and_then(|c| c.unavailable) {
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
        Ok(project)
    }

    pub fn delete_project(&self, id: &str) -> anyhow::Result<()> {
        self.settings.lock().unwrap().projects.retain(|p| p.id != id);
        superviser_core::agent::Chat::delete_all(&self.paths, id);
        self.chat_models.lock().unwrap().retain(|key, _| !key.starts_with(&format!("{id}/")));
        self.save_settings()?;
        self.sync_projects();
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
        let model = self.latest_model().map(|(m, context)| opencode::SetupModel { alias: m.alias, name: m.model.name, context });
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
        Ok(launched)
    }

    pub fn shutdown(&self) {
        self.engines.stop_all();
        if let Ok(mut running) = self.running.try_lock() {
            for (_, mut running) in running.drain() {
                self.remember_pid(&running.server.alias, None);
                tauri::async_runtime::block_on(running.server.stop());
            }
        }
        if let Ok(mut judge) = self.judge_server.try_lock()
            && let Some(mut judge) = judge.take()
        {
            self.remember_pid("judge", None);
            tauri::async_runtime::block_on(judge.stop());
        }
        if let Some(guard) = self.guard.lock().unwrap().as_mut() {
            guard.stop();
        }
    }
}
