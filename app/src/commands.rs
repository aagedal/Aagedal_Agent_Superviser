//! What the settings and chat windows can ask for.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;
use superviser_core::catalog::{CatalogModel, Fit, Offer};
use superviser_core::discover::{Locations, MlxModel, Skipped};
use superviser_core::hardware::Hardware;
use superviser_core::hub::{Hub, SearchHit};
use superviser_core::installed::{self, InstalledModel};
use superviser_core::{opencode, secrets};
use superviser_guard::CallEvent;
use tauri::{AppHandle, Emitter, State};

use crate::state::{App, ServerStatus};

type Result<T> = std::result::Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn hub() -> Hub {
    Hub::new(secrets::hf_token())
}

#[derive(Serialize)]
pub struct Overview {
    hardware: Hardware,
    status: ServerStatus,
    guard_url: Option<String>,
    opencode_version: Option<String>,
    llama_server: Option<String>,
    hf_token_set: bool,
}

#[tauri::command]
pub fn overview(app: State<'_, Arc<App>>) -> Overview {
    Overview {
        hardware: app.hardware.clone(),
        status: app.status.lock().unwrap().clone(),
        guard_url: app.guard_url(),
        opencode_version: opencode::version(),
        llama_server: superviser_core::runtime::find_llama_server().map(|p| p.display().to_string()),
        hf_token_set: secrets::hf_token().is_some(),
    }
}

#[derive(Serialize)]
pub struct Offers {
    offers: Vec<Offer>,
    /// Curated models left out because they don't fit.
    hidden: usize,
}

#[tauri::command]
pub fn catalog_offers(app: State<'_, Arc<App>>) -> Offers {
    let offers = app.catalog.offers(&app.hardware);
    Offers { hidden: app.catalog.models.len() - offers.len(), offers }
}

#[tauri::command]
pub async fn search_models(query: String) -> Result<Vec<SearchHit>> {
    hub().search(&query, 20).await.map_err(err)
}

#[derive(Serialize)]
pub struct Checked {
    model: CatalogModel,
    /// `None` when it doesn't fit this machine: it can't be downloaded.
    fit: Option<Fit>,
}

/// Resolves a search result's default 4-bit quant and whether it fits.
#[tauri::command]
pub async fn check_model(app: State<'_, Arc<App>>, repo: String) -> Result<Checked> {
    let model = CatalogModel::resolve(&hub(), &repo, None, "").await.map_err(|e| format!("{e:#}"))?;
    Ok(Checked { fit: app.fit(&model), model })
}

#[derive(Clone, Serialize)]
struct DownloadEvent {
    id: String,
    done: u64,
    total: u64,
    finished: bool,
    error: Option<String>,
}

#[tauri::command]
pub fn download_model(handle: AppHandle, app: State<'_, Arc<App>>, model: CatalogModel) -> Result<String> {
    if app.fit(&model).is_none() {
        return Err("This model is too large for this machine's memory.".into());
    }
    let id = installed::model_id(&model);
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut downloads = app.downloads.lock().unwrap();
        if downloads.contains_key(&id) {
            return Err("already downloading".into());
        }
        downloads.insert(id.clone(), cancel.clone());
    }
    let app = app.inner().clone();
    let task_id = id.clone();
    tauri::async_runtime::spawn(async move {
        let mut last = std::time::Instant::now();
        let result = installed::install(&hub(), &app.paths, &model, cancel, |p| {
            if last.elapsed().as_millis() >= 250 || p.done == p.total {
                last = std::time::Instant::now();
                let _ = handle.emit("download", DownloadEvent { id: task_id.clone(), done: p.done, total: p.total, finished: false, error: None });
            }
        })
        .await;
        app.downloads.lock().unwrap().remove(&task_id);
        let (finished, error) = match result {
            Ok(_) => (true, None),
            Err(e) => (false, Some(format!("{e:#}"))),
        };
        let _ = handle.emit("download", DownloadEvent { id: task_id, done: 0, total: 0, finished, error });
        crate::refresh_tray(&handle);
    });
    Ok(id)
}

#[tauri::command]
pub fn cancel_download(app: State<'_, Arc<App>>, id: String) {
    if let Some(cancel) = app.downloads.lock().unwrap().get(&id) {
        cancel.store(true, Ordering::Relaxed);
    }
}

#[derive(Serialize)]
pub struct InstalledView {
    #[serde(flatten)]
    model: InstalledModel,
    fit: Option<Fit>,
    source_label: &'static str,
    path: Option<String>,
    /// Only the app's own downloads can be deleted from here.
    removable: bool,
}

#[tauri::command]
pub fn installed_models(app: State<'_, Arc<App>>) -> Vec<InstalledView> {
    app.installed()
        .into_iter()
        .map(|model| InstalledView {
            fit: app.fit(&model.model),
            source_label: model.source.label(),
            path: model.entry_path().map(|p| p.display().to_string()),
            removable: model.source == installed::Source::App,
            model,
        })
        .collect()
}

#[derive(Serialize)]
pub struct DiscoveryInfo {
    enabled: bool,
    found: usize,
    skipped: Vec<Skipped>,
    mlx: Vec<MlxModel>,
    locations: Locations,
}

fn discovery_info(app: &App) -> DiscoveryInfo {
    let discovery = app.discovery.lock().unwrap();
    DiscoveryInfo {
        enabled: app.settings.lock().unwrap().use_other_apps,
        found: discovery.models.len(),
        skipped: discovery.skipped.clone(),
        mlx: discovery.mlx.clone(),
        locations: Locations::detect(),
    }
}

/// What the last scan for other apps' models found and left out.
#[tauri::command]
pub fn other_apps(app: State<'_, Arc<App>>) -> DiscoveryInfo {
    discovery_info(&app)
}

#[tauri::command]
pub async fn rescan_models(handle: AppHandle, app: State<'_, Arc<App>>) -> Result<DiscoveryInfo> {
    let app = app.inner().clone();
    let scanned = app.clone();
    tauri::async_runtime::spawn_blocking(move || scanned.rescan()).await.map_err(err)?;
    crate::refresh_tray(&handle);
    Ok(discovery_info(&app))
}

#[tauri::command]
pub async fn set_use_other_apps(handle: AppHandle, app: State<'_, Arc<App>>, enabled: bool) -> Result<DiscoveryInfo> {
    app.settings.lock().unwrap().use_other_apps = enabled;
    app.save_settings().map_err(err)?;
    rescan_models(handle, app).await
}

#[tauri::command]
pub async fn delete_model(handle: AppHandle, app: State<'_, Arc<App>>, id: String) -> Result<()> {
    if matches!(&*app.status.lock().unwrap(), ServerStatus::Running { model, .. } | ServerStatus::Loading { model, .. } if *model == id) {
        return Err("Stop the model before deleting it.".into());
    }
    app.delete_model(&id).map_err(err)?;
    crate::refresh_tray(&handle);
    Ok(())
}

#[tauri::command]
pub async fn start_model(handle: AppHandle, app: State<'_, Arc<App>>, id: String) -> Result<()> {
    crate::start_model(handle, app.inner().clone(), id).await
}

#[tauri::command]
pub async fn stop_model(handle: AppHandle, app: State<'_, Arc<App>>) -> Result<()> {
    app.stop_model().await;
    crate::status_changed(&handle);
    Ok(())
}

#[derive(Serialize)]
pub struct ApiInfo {
    base_url: Option<String>,
    api_key: String,
    model: Option<String>,
}

#[tauri::command]
pub fn api_info(app: State<'_, Arc<App>>) -> Result<ApiInfo> {
    Ok(ApiInfo {
        base_url: app.guard_url(),
        api_key: secrets::local_api_key(&app.paths).map_err(err)?,
        model: app.running_model().map(|(m, _)| m.alias),
    })
}

/// A new key; harnesses configured with the old one need setting up again.
#[tauri::command]
pub fn regenerate_api_key(app: State<'_, Arc<App>>) -> Result<String> {
    let key = secrets::regenerate_local_api_key(&app.paths).map_err(err)?;
    if let Some(guard) = app.guard.lock().unwrap().as_ref() {
        guard.set_api_key(Some(key.clone()));
    }
    Ok(key)
}

#[tauri::command]
pub fn set_hf_token(token: Option<String>) -> Result<()> {
    secrets::set_hf_token(token.as_deref()).map_err(err)
}

#[tauri::command]
pub fn setup_opencode(app: State<'_, Arc<App>>) -> Result<opencode::Installed> {
    app.setup_opencode().map_err(|e| format!("{e:#}"))
}

#[derive(Serialize)]
pub struct JudgeSettings {
    enabled: bool,
    fail_closed: bool,
}

#[tauri::command]
pub fn judge_settings(app: State<'_, Arc<App>>) -> JudgeSettings {
    let settings = app.settings.lock().unwrap();
    JudgeSettings { enabled: settings.judge, fail_closed: settings.judge_fail_closed }
}

#[tauri::command]
pub fn set_judge(app: State<'_, Arc<App>>, enabled: bool, fail_closed: bool) -> Result<()> {
    app.set_judge(enabled, fail_closed).map_err(err)
}

#[tauri::command]
pub fn recent_calls(app: State<'_, Arc<App>>) -> Vec<CallEvent> {
    app.events.lock().unwrap().iter().cloned().collect()
}

#[derive(Serialize)]
pub struct SandboxInfo {
    available: bool,
    network: superviser_core::sandbox::Network,
    recent_projects: Vec<String>,
    /// Isolation options that installing something would make available.
    installable: Vec<superviser_core::sandbox::Choice>,
}

#[tauri::command]
pub fn sandbox_info(app: State<'_, Arc<App>>) -> SandboxInfo {
    let settings = app.settings.lock().unwrap();
    // Any folder: whether a backend is installed doesn't depend on the project.
    let installable = superviser_core::sandbox::choices(std::path::Path::new("/")).into_iter().filter(|c| c.setup.is_some()).collect();
    SandboxInfo { available: cfg!(target_os = "macos"), network: settings.sandbox_network, recent_projects: settings.recent_projects.clone(), installable }
}

/// Opens the download of what an isolation option needs (Apple's signed `container`
/// installer) in the browser, and says which link it opened.
#[tauri::command]
pub async fn get_installer(isolation: superviser_core::sandbox::Isolation) -> Result<String> {
    let url = match isolation {
        superviser_core::sandbox::Isolation::LinuxVm => superviser_core::sandbox::container::installer_url().await,
        superviser_core::sandbox::Isolation::MacosSandbox => return Err("the macOS sandbox needs nothing installed".into()),
    };
    crate::open_url(&url).map_err(err)?;
    Ok(url)
}

#[tauri::command]
pub fn set_sandbox_network(handle: AppHandle, app: State<'_, Arc<App>>, network: superviser_core::sandbox::Network) -> Result<()> {
    app.settings.lock().unwrap().sandbox_network = network;
    app.save_settings().map_err(err)?;
    crate::refresh_tray(&handle);
    Ok(())
}

#[derive(Serialize)]
pub struct ProjectIsolation {
    current: superviser_core::sandbox::Isolation,
    choices: Vec<superviser_core::sandbox::Choice>,
}

/// How a project is isolated, and the ways it could be.
#[tauri::command]
pub fn project_isolation(app: State<'_, Arc<App>>, path: String) -> ProjectIsolation {
    let project = std::path::Path::new(&path);
    ProjectIsolation { current: app.isolation(project), choices: superviser_core::sandbox::choices(project) }
}

#[tauri::command]
pub fn set_project_isolation(app: State<'_, Arc<App>>, path: String, isolation: superviser_core::sandbox::Isolation) -> Result<()> {
    let choice = superviser_core::sandbox::choices(std::path::Path::new(&path)).into_iter().find(|c| c.isolation == isolation);
    if let Some(why) = choice.and_then(|c| c.unavailable) {
        return Err(why);
    }
    app.settings.lock().unwrap().project_isolation.insert(path, isolation);
    app.save_settings().map_err(err)
}

/// Opens a project (or asks for one, with `path` unset) in isolated OpenCode.
#[tauri::command]
pub fn open_project(handle: AppHandle, path: Option<String>) {
    match path {
        Some(path) => crate::open_project(&handle, path.into()),
        None => crate::choose_project(&handle),
    }
}

#[tauri::command]
pub fn open_window(handle: AppHandle, name: String) -> Result<()> {
    if !matches!(name.as_str(), "settings" | "chat") {
        return Err(format!("no window called {name}"));
    }
    crate::open_window(&handle, &name).map_err(err)
}
