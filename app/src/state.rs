//! The app's state and what it can do, independent of windows and menus.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
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
    running: tokio::sync::Mutex<Option<Running>>,
    pub status: Mutex<ServerStatus>,
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
}

impl App {
    pub fn new() -> anyhow::Result<Self> {
        let paths = Paths::new()?;
        superviser_core::paths::private_dir(&paths.data)?;
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
            running: tokio::sync::Mutex::new(None),
            status: Mutex::new(ServerStatus::Stopped),
            judge_server: tokio::sync::Mutex::new(None),
            judge_status: Mutex::new(JudgeStatus::Idle),
            downloads: Mutex::new(HashMap::new()),
            events: Mutex::new(VecDeque::new()),
            discovery: Mutex::new(Discovery::default()),
            chats: crate::agent::Chats::default(),
            engines: crate::acp::Engines::default(),
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
        if enabled
            && !self.settings.lock().unwrap().judge
            && let Some((model, context)) = self.running_model()
        {
            let left = self.hardware.less(judge.judge_required());
            if model.model.fit(left.model_budget).is_none_or(|fit| fit.context < context) {
                bail!(
                    "{} needs {:.1} GB beside {}, which doesn't fit at {}k context. Stop the model, turn the judge on, and start it again: it will get the context that fits.",
                    judge.name,
                    judge.judge_required() as f64 / 1e9,
                    model.model.name,
                    context / 1024
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
        let wanted = if enabled && self.running_model().is_some() { self.judge_installed() } else { None };
        let mut server = self.judge_server.lock().await;
        let Some(model) = wanted else {
            if let Some(mut old) = server.take() {
                old.stop().await;
            }
            self.set_guard_judge(None);
            *self.judge_status.lock().unwrap() = JudgeStatus::Idle;
            return;
        };
        if server.as_mut().is_none_or(|s| !s.is_running()) {
            *self.judge_status.lock().unwrap() = JudgeStatus::Loading;
            match self.start_judge(&model).await {
                Ok(started) => *server = Some(started),
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

    /// Loads `id` in llama-server and points the guard (and OpenCode) at it.
    pub async fn start_model(&self, id: &str, mut on_status: impl FnMut(&ServerStatus)) -> anyhow::Result<()> {
        let model = self.find_installed(id).with_context(|| format!("{id} is not installed"))?;
        let mut set_status = |status: ServerStatus| {
            *self.status.lock().unwrap() = status.clone();
            on_status(&status);
        };
        let result = async {
            let fit = self.fit(&model.model).context("this model no longer fits in this machine's memory")?;
            let binary = runtime::find_llama_server()
                .context("llama-server was not found (install llama.cpp, or set SUPERVISER_LLAMA_SERVER)")?;
            let entry = model.entry_path().context("model has no entry file")?;
            secrets::upstream_api_key(&self.paths)?;

            let mut running = self.running.lock().await;
            if let Some(mut old) = running.take() {
                old.server.stop().await;
            }
            set_status(ServerStatus::Loading { model: model.id.clone(), name: model.model.name.clone() });
            let prompt_cache = model.model.prompt_cache(&fit, self.model_hardware().ram_budget);
            tracing::info!("{}: {}k context, {} MiB for saved conversations", model.alias, fit.context / 1024, prompt_cache >> 20);
            let config = ServerConfig {
                binary,
                model_path: entry,
                alias: model.alias.clone(),
                context: fit.context,
                port: runtime::free_port()?,
                api_key_file: self.paths.upstream_api_key(),
                log_file: self.paths.logs().join("llama-server.log"),
                prompt_cache,
                decision: false,
            };
            let mut server = LlamaServer::start(config).await?;
            if let Err(e) = server.wait_ready(LOAD_TIMEOUT).await {
                server.stop().await;
                return Err(e.context(format!("see {}", self.paths.logs().join("llama-server.log").display())));
            }
            if let Some(guard) = self.guard.lock().unwrap().as_ref() {
                guard.set_upstream(Some(server.base_url()));
            }
            *running = Some(Running { server });
            anyhow::Ok(fit)
        }
        .await;
        match result {
            Ok(fit) => {
                set_status(ServerStatus::Running { model: model.id.clone(), name: model.model.name.clone(), context: fit.context });
                self.settings.lock().unwrap().last_model = Some(model.id.clone());
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
                set_status(ServerStatus::Failed { model: model.id.clone(), error: format!("{e:#}") });
                self.sync_judge().await;
                Err(e)
            }
        }
    }

    pub async fn stop_model(&self) {
        if let Some(mut running) = self.running.lock().await.take() {
            running.server.stop().await;
        }
        if let Some(guard) = self.guard.lock().unwrap().as_ref() {
            guard.set_upstream(None);
        }
        *self.status.lock().unwrap() = ServerStatus::Stopped;
        self.sync_judge().await;
    }

    /// The running model, if any: (installed model, context).
    pub fn running_model(&self) -> Option<(InstalledModel, u64)> {
        let ServerStatus::Running { model, context, .. } = self.status.lock().unwrap().clone() else { return None };
        self.find_installed(&model).map(|m| (m, context))
    }

    /// Adds the running model to OpenCode's global config, as its default.
    pub fn setup_opencode(&self) -> anyhow::Result<opencode::Installed> {
        let Some((model, context)) = self.running_model() else { bail!("start a model first") };
        let base_url = self.guard_url().context("the guard is not running")?;
        secrets::local_api_key(&self.paths)?;
        let setup = opencode::Setup {
            base_url,
            api_key_file: self.paths.local_api_key(),
            models: vec![opencode::SetupModel { alias: model.alias.clone(), name: model.model.name.clone(), context }],
            default_model: Some(model.alias),
        };
        opencode::install(&setup)
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
        if self.running_model().is_none() {
            bail!("Start a model first: OpenCode would have nothing to talk to.");
        }
        self.setup_opencode()?;
        let guard_url = self.guard_url().context("the guard is not running")?;
        let model = self.running_model().map(|(m, context)| opencode::SetupModel { alias: m.alias, name: m.model.name, context });
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
        if let Ok(mut running) = self.running.try_lock()
            && let Some(mut running) = running.take()
        {
            tauri::async_runtime::block_on(running.server.stop());
        }
        if let Ok(mut judge) = self.judge_server.try_lock()
            && let Some(mut judge) = judge.take()
        {
            tauri::async_runtime::block_on(judge.stop());
        }
        if let Some(guard) = self.guard.lock().unwrap().as_mut() {
            guard.stop();
        }
    }
}
