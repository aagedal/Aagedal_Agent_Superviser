//! The app's state and what it can do, independent of windows and menus.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use superviser_core::catalog::{Catalog, CatalogModel, Fit};
use superviser_core::discover::{self, Discovery, Locations};
use superviser_core::hardware::Hardware;
use superviser_core::installed::{self, InstalledModel, Source};
use superviser_core::paths::{PROTECTED_NAMES, Paths};
use superviser_core::runtime::{self, LlamaServer, ServerConfig};
use superviser_core::sandbox::{self, Isolation, Network};
use superviser_core::{opencode, secrets};
use superviser_guard::journal::Journal;
use superviser_guard::selfprotect::SelfProtection;
use superviser_guard::{CallEvent, GuardServer, ProxyOptions};

pub const DEFAULT_GUARD_PORT: u16 = 8787;
const MAX_RECENT_PROJECTS: usize = 6;
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
    /// Write OpenCode's config whenever a model starts.
    pub configure_opencode: bool,
    /// Offer models other apps already downloaded (LM Studio, Ollama, Hugging Face cache…).
    pub use_other_apps: bool,
    pub last_model: Option<String>,
    /// Internet access inside the OpenCode sandbox.
    pub sandbox_network: Network,
    /// Projects opened in OpenCode, most recent first.
    pub recent_projects: Vec<String>,
    /// How each project is isolated, when not the default macOS sandbox.
    pub project_isolation: HashMap<String, Isolation>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            guard_port: DEFAULT_GUARD_PORT,
            block: true,
            configure_opencode: true,
            use_other_apps: true,
            last_model: None,
            sandbox_network: Network::default(),
            recent_projects: Vec::new(),
            project_isolation: HashMap::new(),
        }
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

pub struct App {
    pub paths: Paths,
    pub hardware: Hardware,
    pub catalog: Catalog,
    pub settings: Mutex<Settings>,
    pub guard: Mutex<Option<GuardServer>>,
    running: tokio::sync::Mutex<Option<Running>>,
    pub status: Mutex<ServerStatus>,
    pub downloads: Mutex<HashMap<String, Arc<AtomicBool>>>,
    pub events: Mutex<VecDeque<CallEvent>>,
    /// Models other apps downloaded, from the last scan.
    pub discovery: Mutex<Discovery>,
}

impl App {
    pub fn new() -> anyhow::Result<Self> {
        let paths = Paths::new()?;
        superviser_core::paths::private_dir(&paths.data)?;
        let settings = std::fs::read_to_string(paths.settings())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Ok(Self {
            hardware: Hardware::detect(),
            catalog: Catalog::bundled(),
            settings: Mutex::new(settings),
            guard: Mutex::new(None),
            running: tokio::sync::Mutex::new(None),
            status: Mutex::new(ServerStatus::Stopped),
            downloads: Mutex::new(HashMap::new()),
            events: Mutex::new(VecDeque::new()),
            discovery: Mutex::new(Discovery::default()),
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
        };
        let server = GuardServer::start(listener, config, options).await?;
        tracing::info!("guard listening on {}", server.addr);
        // Linux VMs reach the guard through this socket, never over the network.
        #[cfg(unix)]
        if let Err(e) = server.listen_unix(&self.paths.guard_socket()) {
            tracing::warn!("guard socket {}: {e}", self.paths.guard_socket().display());
        }
        *self.guard.lock().unwrap() = Some(server);
        Ok(())
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
        model.fit(self.hardware.model_budget)
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
            let config = ServerConfig {
                binary,
                model_path: entry,
                alias: model.alias.clone(),
                context: fit.context,
                port: runtime::free_port()?,
                api_key_file: self.paths.upstream_api_key(),
                log_file: self.paths.logs().join("llama-server.log"),
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
                if self.settings.lock().unwrap().configure_opencode
                    && let Err(e) = self.setup_opencode()
                {
                    tracing::warn!("OpenCode setup: {e:#}");
                }
                Ok(())
            }
            Err(e) => {
                set_status(ServerStatus::Failed { model: model.id.clone(), error: format!("{e:#}") });
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

    /// How `project` is isolated: its saved choice, or the macOS sandbox.
    pub fn isolation(&self, project: &std::path::Path) -> Isolation {
        self.settings.lock().unwrap().project_isolation.get(&project.display().to_string()).copied().unwrap_or_default()
    }

    /// Opens `project` in OpenCode, in a new Terminal window, isolated as chosen for it.
    /// OpenCode's config is refreshed first so it talks to the guard about the running model.
    pub fn open_project(&self, project: &std::path::Path) -> anyhow::Result<sandbox::Launched> {
        if self.running_model().is_none() {
            bail!("Start a model first: OpenCode would have nothing to talk to.");
        }
        self.setup_opencode()?;
        let network = self.settings.lock().unwrap().sandbox_network;
        let model = self.running_model().map(|(m, context)| opencode::SetupModel { alias: m.alias, name: m.model.name, context });
        let launched = sandbox::launch(&self.paths, project, self.isolation(project), network, model)?;
        {
            let mut settings = self.settings.lock().unwrap();
            let path = project.display().to_string();
            settings.recent_projects.retain(|p| *p != path);
            settings.recent_projects.insert(0, path);
            settings.recent_projects.truncate(MAX_RECENT_PROJECTS);
        }
        self.save_settings()?;
        Ok(launched)
    }

    pub fn shutdown(&self) {
        if let Ok(mut running) = self.running.try_lock()
            && let Some(mut running) = running.take()
        {
            tauri::async_runtime::block_on(running.server.stop());
        }
        if let Some(guard) = self.guard.lock().unwrap().as_mut() {
            guard.stop();
        }
    }
}
