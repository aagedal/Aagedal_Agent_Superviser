//! What the settings and chat windows can ask for.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;
use superviser_core::agent::{Chat, ChatSummary};
use superviser_core::catalog::{Catalog, CatalogModel, Fit, Offer, Pick};
use superviser_core::discover::{Locations, MlxModel, Skipped};
use superviser_core::hardware::Hardware;
use superviser_core::hub::{Hub, SearchHit};
use superviser_core::installed::{self, InstalledModel};
use superviser_core::project::{Project, SUGGESTED_PROGRAMS, TOOL_GROUPS, ToolGroup};
use superviser_core::{opencode, secrets};
use superviser_guard::CallEvent;
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_dialog::DialogExt;

use crate::state::{App, JudgeStatus, LoadState, LoadedModel, Room, ServerStatus};

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
    /// What a model may take now: less while the judge is on.
    model_budget: u64,
    /// The most recently used loaded model, or one loading or failed (see [`App::status`]).
    status: ServerStatus,
    /// Every loaded, loading or failed model, most recently used first.
    models: Vec<LoadedModel>,
    guard_url: Option<String>,
    opencode_version: Option<String>,
    llama_server: Option<String>,
    hf_token_set: bool,
}

#[tauri::command]
pub fn overview(app: State<'_, Arc<App>>) -> Overview {
    Overview {
        hardware: app.hardware.clone(),
        model_budget: app.model_hardware().model_budget,
        status: app.status(),
        models: app.models(),
        guard_url: app.guard_url(),
        opencode_version: opencode::version(),
        llama_server: superviser_core::runtime::find_llama_server().map(|p| p.display().to_string()),
        hf_token_set: secrets::hf_token().is_some(),
    }
}

#[derive(Serialize)]
pub struct Offers {
    /// The short list shown first.
    picks: Vec<Pick>,
    offers: Vec<Offer>,
    /// Curated models left out because they don't fit.
    hidden: usize,
}

#[tauri::command]
pub fn catalog_offers(app: State<'_, Arc<App>>) -> Offers {
    let offers = app.catalog.offers(&app.model_hardware());
    let picks = Catalog::picks(&offers, app.hardware.total_memory);
    Offers { hidden: app.catalog.models.len() - offers.len(), picks, offers }
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

/// The download id of the judge's model.
const JUDGE_DOWNLOAD: &str = "judge";

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
    if app.models().iter().any(|m| m.id == id && !matches!(m.state, LoadState::Failed { .. })) {
        return Err("Unload the model before deleting it.".into());
    }
    app.delete_model(&id).map_err(err)?;
    crate::refresh_tray(&handle);
    Ok(())
}

/// Loads a model beside the loaded ones, if it fits (see `room_for`).
#[tauri::command]
pub async fn load_model(handle: AppHandle, app: State<'_, Arc<App>>, id: String) -> Result<()> {
    crate::load_model(handle, app.inner().clone(), id).await
}

/// Unloads a model, or every model without `id`.
#[tauri::command]
pub async fn unload_model(handle: AppHandle, app: State<'_, Arc<App>>, id: Option<String>) -> Result<()> {
    app.unload_model(id.as_deref()).await;
    crate::status_changed(&handle);
    Ok(())
}

/// Whether a model can load beside the loaded ones, else which to unload first.
#[tauri::command]
pub fn room_for(app: State<'_, Arc<App>>, id: String) -> Result<Room> {
    app.room_for(&id).map_err(|e| format!("{e:#}"))
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
        model: app.latest_model().map(|(m, _)| m.alias),
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
    /// The judge's model; `None` when this version has none.
    model: Option<CatalogModel>,
    /// Memory it takes while it runs.
    memory: u64,
    installed: bool,
    /// False when it doesn't fit beside even the smallest model.
    fits: bool,
    status: JudgeStatus,
    /// Its download id, for progress events.
    download: &'static str,
}

#[tauri::command]
pub fn judge_settings(app: State<'_, Arc<App>>) -> JudgeSettings {
    let (enabled, fail_closed) = {
        let settings = app.settings.lock().unwrap();
        (settings.judge, settings.judge_fail_closed)
    };
    JudgeSettings {
        enabled,
        fail_closed,
        memory: app.catalog.judge.as_ref().map_or(0, CatalogModel::judge_required),
        model: app.catalog.judge.clone(),
        installed: app.judge_installed().is_some(),
        fits: app.judge_fits(),
        status: app.judge_status.lock().unwrap().clone(),
        download: JUDGE_DOWNLOAD,
    }
}

#[tauri::command]
pub async fn set_judge(handle: AppHandle, app: State<'_, Arc<App>>, enabled: bool, fail_closed: bool) -> Result<()> {
    let result = app.set_judge(enabled, fail_closed).await.map_err(|e| format!("{e:#}"));
    // What fits, and so what Settings offers, changed.
    crate::refresh_tray(&handle);
    let _ = handle.emit("judge", ());
    result
}

/// Downloads the judge's model, then starts it if the judge is on and a model runs.
#[tauri::command]
pub fn download_judge(handle: AppHandle, app: State<'_, Arc<App>>) -> Result<()> {
    let model = app.catalog.judge.clone().ok_or("this version has no judge model")?;
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut downloads = app.downloads.lock().unwrap();
        if downloads.contains_key(JUDGE_DOWNLOAD) {
            return Err("already downloading".into());
        }
        downloads.insert(JUDGE_DOWNLOAD.into(), cancel.clone());
    }
    let app = app.inner().clone();
    tauri::async_runtime::spawn(async move {
        let mut last = std::time::Instant::now();
        let dir = installed::judge_dir(&app.paths, &model);
        let result = installed::install_to(&hub(), &dir, &model, cancel, |p| {
            if last.elapsed().as_millis() >= 250 || p.done == p.total {
                last = std::time::Instant::now();
                let _ = handle.emit("download", DownloadEvent { id: JUDGE_DOWNLOAD.into(), done: p.done, total: p.total, finished: false, error: None });
            }
        })
        .await;
        app.downloads.lock().unwrap().remove(JUDGE_DOWNLOAD);
        let (finished, error) = match result {
            Ok(_) => (true, None),
            Err(e) => (false, Some(format!("{e:#}"))),
        };
        let _ = handle.emit("download", DownloadEvent { id: JUDGE_DOWNLOAD.into(), done: 0, total: 0, finished, error });
        if finished {
            app.sync_judge().await;
        }
        let _ = handle.emit("judge", ());
    });
    Ok(())
}

#[tauri::command]
pub fn recent_calls(app: State<'_, Arc<App>>) -> Vec<CallEvent> {
    app.events.lock().unwrap().iter().cloned().collect()
}

#[derive(Serialize)]
pub struct ProjectView {
    #[serde(flatten)]
    project: Project,
    /// The ways its main folder could be isolated.
    choices: Vec<superviser_core::sandbox::Choice>,
}

#[derive(Serialize)]
pub struct ProjectsInfo {
    /// Whether projects can be opened isolated on this OS at all.
    available: bool,
    projects: Vec<ProjectView>,
    /// Isolation options that installing something would make available.
    installable: Vec<superviser_core::sandbox::Choice>,
    tool_groups: &'static [ToolGroup],
    suggested_programs: &'static [&'static str],
    /// MCP servers in OpenCode's global config, which a project can turn off.
    mcp_servers: Vec<String>,
}

#[tauri::command]
pub fn projects_info(app: State<'_, Arc<App>>) -> ProjectsInfo {
    let projects = app.settings.lock().unwrap().projects.clone();
    // Any folder: whether a backend is installed doesn't depend on the project.
    let installable = superviser_core::sandbox::choices(std::path::Path::new("/")).into_iter().filter(|c| c.setup.is_some()).collect();
    ProjectsInfo {
        available: cfg!(target_os = "macos"),
        projects: projects.into_iter().map(|p| ProjectView { choices: superviser_core::sandbox::choices(p.main()), project: p }).collect(),
        installable,
        tool_groups: TOOL_GROUPS,
        suggested_programs: SUGGESTED_PROGRAMS,
        mcp_servers: opencode::mcp_servers().into_iter().map(|s| s.name).collect(),
    }
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

/// Asks for a folder; `None` if the dialog was cancelled.
#[tauri::command]
pub async fn choose_folder(handle: AppHandle, title: String) -> Option<String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut dialog = handle.dialog().file().set_title(title);
    if let Some(home) = std::env::var_os("HOME") {
        dialog = dialog.set_directory(home);
    }
    dialog.pick_folder(move |folder| {
        let _ = tx.send(folder.and_then(|f| f.into_path().ok()));
    });
    rx.await.ok().flatten().map(|p| p.display().to_string())
}

/// The project for `folder`, made if there is none.
#[tauri::command]
pub fn new_project(handle: AppHandle, app: State<'_, Arc<App>>, folder: String) -> Result<Project> {
    let project = app.project_for_folder(std::path::Path::new(&folder)).map_err(err)?;
    projects_changed(&handle);
    Ok(project)
}

#[tauri::command]
pub fn save_project(handle: AppHandle, app: State<'_, Arc<App>>, project: Project) -> Result<Project> {
    let project = app.save_project(project).map_err(err)?;
    projects_changed(&handle);
    Ok(project)
}

#[tauri::command]
pub fn delete_project(handle: AppHandle, app: State<'_, Arc<App>>, id: String) -> Result<()> {
    app.delete_project(&id).map_err(err)?;
    projects_changed(&handle);
    Ok(())
}

fn projects_changed(handle: &AppHandle) {
    let _ = handle.emit("projects", ());
    crate::refresh_tray(handle);
}

/// Opens a project (or asks for a folder, with `id` unset) in isolated OpenCode.
#[tauri::command]
pub fn open_project(handle: AppHandle, id: Option<String>) {
    match id {
        Some(id) => crate::open_project(&handle, &id),
        None => crate::choose_project(&handle),
    }
}

// --- The chat window's engine: OpenCode over ACP, else the built-in agent ---

/// `opencode` when OpenCode is bundled or installed, else `builtin`.
#[tauri::command]
pub fn chat_engine() -> &'static str {
    if opencode::binary().is_some() { "opencode" } else { "builtin" }
}

fn project(app: &App, id: &str) -> Result<superviser_core::project::Project> {
    app.project(id).ok_or_else(|| "that project no longer exists".to_string())
}

/// The project's saved OpenCode sessions, most recent first.
#[tauri::command]
pub async fn acp_sessions(handle: AppHandle, app: State<'_, Arc<App>>, project: String) -> Result<serde_json::Value> {
    let project = self::project(&app, &project)?;
    let engine = app.engines.get(&handle, &app, &project).await.map_err(|e| format!("{e:#}"))?;
    engine.sessions().await.map_err(|e| format!("{e:#}"))
}

#[derive(Serialize)]
pub struct Opened {
    session: String,
    busy: bool,
}

/// Opens a saved session (replayed as events) or, without `session`, a new one.
#[tauri::command]
pub async fn acp_open(handle: AppHandle, app: State<'_, Arc<App>>, project: String, session: Option<String>) -> Result<Opened> {
    let project = self::project(&app, &project)?;
    let engine = app.engines.get(&handle, &app, &project).await.map_err(|e| format!("{e:#}"))?;
    let session = match session {
        Some(session) => {
            if !engine.is_busy(&session) {
                engine.load_session(&session).await.map_err(|e| format!("{e:#}"))?;
            }
            session
        }
        None => engine.new_session().await.map_err(|e| format!("{e:#}"))?,
    };
    Ok(Opened { busy: engine.is_busy(&session), session })
}

#[tauri::command]
pub async fn acp_prompt(handle: AppHandle, app: State<'_, Arc<App>>, project: String, session: String, text: String, model: String) -> Result<()> {
    let (model, _) = app.loaded(&model).ok_or("Load the chat's model first.")?;
    app.touch(&model.id);
    let project = self::project(&app, &project)?;
    let engine = app.engines.get(&handle, &app, &project).await.map_err(|e| format!("{e:#}"))?;
    engine.prompt(handle, &session, &text, &model.alias).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn acp_cancel(app: State<'_, Arc<App>>, project: String, session: String) -> Result<()> {
    match app.engines.running(&project).await {
        Some(engine) => engine.cancel(&session).await.map_err(|e| format!("{e:#}")),
        None => Ok(()),
    }
}

#[tauri::command]
pub async fn acp_permission(app: State<'_, Arc<App>>, project: String, request: String, option: Option<String>) -> Result<()> {
    let engine = app.engines.running(&project).await.ok_or("OpenCode isn't running for this project")?;
    engine.answer(&request, option.as_deref()).await.map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn acp_delete(handle: AppHandle, app: State<'_, Arc<App>>, project: String, session: String) -> Result<()> {
    let project = self::project(&app, &project)?;
    let engine = app.engines.get(&handle, &app, &project).await.map_err(|e| format!("{e:#}"))?;
    engine.delete_session(&session).await.map_err(|e| format!("{e:#}"))?;
    app.set_chat_model(&project.id, &session, None).map_err(err)
}

/// The installed model a chat uses, if one was chosen for it.
#[tauri::command]
pub fn chat_model(app: State<'_, Arc<App>>, project: String, chat: String) -> Option<String> {
    app.chat_model(&project, &chat)
}

#[tauri::command]
pub fn set_chat_model(app: State<'_, Arc<App>>, project: String, chat: String, model: String) -> Result<()> {
    if app.find_installed(&model).is_none() {
        return Err(format!("{model} is not installed"));
    }
    app.set_chat_model(&project, &chat, Some(model)).map_err(err)
}

// --- Opening the project's files ---

/// Editors offered in "Open with", if installed.
const EDITORS: &[&str] = &["Visual Studio Code", "Cursor", "Zed", "Sublime Text", "BBEdit", "Nova", "Xcode", "TextEdit"];

/// Kinds of file whose default app runs them: opening one would run the agent's code
/// outside the sandbox, so they only open in an editor or Finder.
const RUNS_CODE: &[&str] = &[
    "app", "command", "sh", "bash", "zsh", "fish", "csh", "ksh", "tool", "terminal", "scpt", "scptd", "applescript", "workflow",
    "action", "shortcut", "pkg", "mpkg", "dmg", "jar", "py", "pyw", "rb", "pl", "php", "url", "webloc", "inetloc", "fileloc",
    "osax", "prefpane", "kext", "plugin", "bundle", "dylib", "so", "mobileconfig",
];

fn installed_app(name: &str) -> bool {
    let home = std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join("Applications"));
    ["/Applications".into(), "/System/Applications".into()].into_iter().chain(home).any(|dir: std::path::PathBuf| dir.join(format!("{name}.app")).exists())
}

/// Whether opening `path` in its default app could run it.
fn runs_code(path: &std::path::Path) -> bool {
    let extension = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if RUNS_CODE.contains(&extension.as_str()) {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if path.is_file() && std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0) {
            return true;
        }
    }
    false
}

#[derive(Serialize)]
pub struct FileInfo {
    /// Its full path.
    path: String,
    /// Its default app might run it; offer an editor or Finder instead.
    runs_code: bool,
    is_dir: bool,
}

/// A project file the chat mentions, if it exists in the project's folders.
#[tauri::command]
pub fn project_file(app: State<'_, Arc<App>>, project: String, path: String) -> Option<FileInfo> {
    let found = app.project(&project)?.find(&path)?;
    Some(FileInfo { runs_code: runs_code(&found), is_dir: found.is_dir(), path: found.display().to_string() })
}

/// The editors installed, for "Open with".
#[tauri::command]
pub fn editors() -> Vec<&'static str> {
    EDITORS.iter().copied().filter(|name| installed_app(name)).collect()
}

/// Opens a project file in its default app (`how` = `default`), in one of [`EDITORS`]
/// (`editor`, with `editor` set), or shows it in Finder (`finder`).
#[tauri::command]
pub fn open_file(app: State<'_, Arc<App>>, project: String, path: String, how: String, editor: Option<String>) -> Result<()> {
    let project = self::project(&app, &project)?;
    let file = project.find(&path).ok_or_else(|| format!("{path} isn't in this project's folders"))?;
    let mut command = std::process::Command::new("/usr/bin/open");
    match how.as_str() {
        "finder" => {
            command.arg("-R");
        }
        "editor" => {
            let editor = editor.filter(|e| EDITORS.contains(&e.as_str()) && installed_app(e)).ok_or("that editor isn't installed")?;
            command.args(["-a", &editor]);
        }
        _ if runs_code(&file) => return Err(format!("{} could run as a program, outside the sandbox. Open it in an editor or show it in Finder.", file.display())),
        _ => {}
    }
    let status = command.arg(&file).status().map_err(err)?;
    if status.success() { Ok(()) } else { Err(format!("couldn't open {}", file.display())) }
}

// --- The built-in agent ---

#[tauri::command]
pub fn chat_list(app: State<'_, Arc<App>>, project: String) -> Vec<ChatSummary> {
    Chat::list(&app.paths, &project)
}

#[derive(Serialize)]
pub struct ChatView {
    #[serde(flatten)]
    chat: Chat,
    /// The agent is working on it now.
    busy: bool,
}

#[tauri::command]
pub fn chat_load(app: State<'_, Arc<App>>, project: String, id: String) -> Result<ChatView> {
    let chat = Chat::load(&app.paths, &project, &id).map_err(err)?;
    Ok(ChatView { busy: app.chats.busy(&chat.id), chat })
}

/// Sends a message (in a new chat without `chat`) and returns the chat's id.
#[tauri::command]
pub fn chat_send(handle: AppHandle, app: State<'_, Arc<App>>, project: String, chat: Option<String>, text: String, model: String) -> Result<String> {
    crate::agent::send(handle, app.inner().clone(), &project, chat.as_deref(), &text, &model).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub fn chat_stop(app: State<'_, Arc<App>>, chat: String) {
    app.chats.stop(&chat);
}

#[tauri::command]
pub fn chat_answer(app: State<'_, Arc<App>>, chat: String, call: String, allow: bool) {
    app.chats.answer(&chat, &call, allow);
}

#[tauri::command]
pub fn chat_delete(app: State<'_, Arc<App>>, project: String, id: String) -> Result<()> {
    app.chats.stop(&id);
    Chat::delete(&app.paths, &project, &id).map_err(err)?;
    app.set_chat_model(&project, &id, None).map_err(err)
}

/// Opens the chat window on a project (the most recent one without `project`).
#[tauri::command]
pub fn open_chat(handle: AppHandle, project: Option<String>) -> Result<()> {
    crate::open_chat(&handle, project.as_deref()).map_err(err)
}

/// Whether the first-run guide was finished or skipped.
#[tauri::command]
pub fn setup_done(app: State<'_, Arc<App>>) -> bool {
    app.settings.lock().unwrap().setup_done
}

#[tauri::command]
pub fn set_setup_done(app: State<'_, Arc<App>>, done: bool) -> Result<()> {
    app.settings.lock().unwrap().setup_done = done;
    app.save_settings().map_err(err)
}

#[tauri::command]
pub fn open_window(handle: AppHandle, name: String) -> Result<()> {
    if !matches!(name.as_str(), "settings" | "chat") {
        return Err(format!("no window called {name}"));
    }
    crate::open_window(&handle, &name).map_err(err)
}
