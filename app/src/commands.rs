//! What the settings and chat windows can ask for.

use std::sync::Arc;

use serde::Serialize;
use superviser_backend::files::{self, FileInfo};
use superviser_backend::git::{self, CommitResult, GitStatus, PrDraft, PullRequestInfo, PushResult};
use superviser_backend::{App, JUDGE_DOWNLOAD, JudgeStatus, LoadedModel, Room, ServerStatus, SubagentRecord};
use superviser_core::agent::{Chat, ChatSummary};
use superviser_core::catalog::{Catalog, CatalogModel, Fit, Offer, Pick};
use superviser_core::discover::{Locations, MlxModel, Skipped};
use superviser_core::hardware::Hardware;
use superviser_core::hub::SearchHit;
use superviser_core::installed::{self, InstalledModel};
use superviser_core::project::{Project, SUGGESTED_PROGRAMS, TOOL_GROUPS, ToolGroup};
use superviser_core::{opencode, secrets};
use superviser_guard::CallEvent;
use tauri::{AppHandle, State};
use tauri_plugin_dialog::DialogExt;

type Result<T> = std::result::Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
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
    include_image_paths: bool,
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
        include_image_paths: app.settings().include_image_paths,
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
    App::hub().search(&query, 20).await.map_err(err)
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
    let model = CatalogModel::resolve(&App::hub(), &repo, None, "").await.map_err(|e| format!("{e:#}"))?;
    Ok(Checked { fit: app.fit(&model), model })
}

#[tauri::command]
pub async fn download_model(app: State<'_, Arc<App>>, model: CatalogModel) -> Result<String> {
    app.inner().start_download_model(model).map_err(err)
}

#[tauri::command]
pub fn cancel_download(app: State<'_, Arc<App>>, id: String) {
    app.cancel_download(&id);
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
pub async fn rescan_models(app: State<'_, Arc<App>>) -> Result<DiscoveryInfo> {
    let app = app.inner().clone();
    let scanned = app.clone();
    tauri::async_runtime::spawn_blocking(move || scanned.rescan_and_notify())
        .await
        .map_err(err)?;
    Ok(discovery_info(&app))
}

#[tauri::command]
pub async fn set_use_other_apps(app: State<'_, Arc<App>>, enabled: bool) -> Result<DiscoveryInfo> {
    app.settings.lock().unwrap().use_other_apps = enabled;
    app.save_settings().map_err(err)?;
    rescan_models(app).await
}

#[derive(Serialize)]
pub struct SavedConversations {
    enabled: bool,
    #[serde(flatten)]
    summary: superviser_core::conversations::Summary,
}

fn saved_conversations_info(app: &App) -> SavedConversations {
    SavedConversations {
        enabled: app.settings.lock().unwrap().save_conversations,
        summary: app.conversations.summary(),
    }
}

/// Each model's last conversation on disk: whether it's kept, and what it takes.
#[tauri::command]
pub fn saved_conversations(app: State<'_, Arc<App>>) -> SavedConversations {
    saved_conversations_info(&app)
}

#[tauri::command]
pub fn set_save_conversations(app: State<'_, Arc<App>>, enabled: bool) -> Result<SavedConversations> {
    app.set_save_conversations(enabled).map_err(err)?;
    Ok(saved_conversations_info(&app))
}

#[tauri::command]
pub fn set_include_image_paths(app: State<'_, Arc<App>>, enabled: bool) -> Result<bool> {
    app.set_include_image_paths(enabled).map_err(err)?;
    Ok(enabled)
}

#[tauri::command]
pub fn clear_saved_conversations(app: State<'_, Arc<App>>) -> SavedConversations {
    app.conversations.clear();
    saved_conversations_info(&app)
}

#[tauri::command]
pub async fn delete_model(app: State<'_, Arc<App>>, id: String) -> Result<()> {
    app.delete_model(&id).map_err(err)
}

/// Loads a model beside the loaded ones, if it fits (see `room_for`).
#[tauri::command]
pub async fn load_model(app: State<'_, Arc<App>>, id: String) -> Result<()> {
    crate::load_model(app.inner().clone(), id).await
}

/// Unloads a model, or every model without `id`.
#[tauri::command]
pub async fn unload_model(app: State<'_, Arc<App>>, id: Option<String>) -> Result<()> {
    app.unload_model(id.as_deref()).await;
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
    app.regenerate_api_key().map_err(err)
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
pub async fn set_judge(app: State<'_, Arc<App>>, enabled: bool, fail_closed: bool) -> Result<()> {
    app.set_judge(enabled, fail_closed).await.map_err(|e| format!("{e:#}"))
}

/// Downloads the judge's model, then starts it if the judge is on and a model runs.
#[tauri::command]
pub async fn download_judge(app: State<'_, Arc<App>>) -> Result<()> {
    app.inner().start_download_judge().map_err(err)
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
    let installable = superviser_core::sandbox::choices(std::path::Path::new("/"))
        .into_iter()
        .filter(|c| c.setup.is_some())
        .collect();
    ProjectsInfo {
        available: cfg!(target_os = "macos"),
        projects: projects
            .into_iter()
            .map(|p| ProjectView { choices: superviser_core::sandbox::choices(p.main()), project: p })
            .collect(),
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
    files::open_url(&url).map_err(err)?;
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
pub fn new_project(app: State<'_, Arc<App>>, folder: String) -> Result<Project> {
    app.project_for_folder(std::path::Path::new(&folder)).map_err(err)
}

#[tauri::command]
pub fn save_project(app: State<'_, Arc<App>>, project: Project) -> Result<Project> {
    app.save_project(project).map_err(err)
}

#[tauri::command]
pub fn delete_project(app: State<'_, Arc<App>>, id: String) -> Result<()> {
    app.delete_project(&id).map_err(err)
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

fn project(app: &App, id: &str) -> Result<Project> {
    app.project(id).ok_or_else(|| "that project no longer exists".to_string())
}

/// The project's saved OpenCode sessions, most recent first.
#[tauri::command]
pub async fn acp_sessions(app: State<'_, Arc<App>>, project: String) -> Result<serde_json::Value> {
    let project = self::project(&app, &project)?;
    let engine = app.engines.get(&app, &project).await.map_err(|e| format!("{e:#}"))?;
    engine.sessions().await.map_err(|e| format!("{e:#}"))
}

#[derive(Serialize)]
pub struct ProjectSessions {
    project: String,
    sessions: serde_json::Value,
    error: Option<String>,
}

/// Saved OpenCode sessions across all projects (without booting dormant Linux VMs).
#[tauri::command]
pub async fn acp_all_sessions(
    app: State<'_, Arc<App>>,
    active_project: Option<String>,
) -> Vec<ProjectSessions> {
    let projects = app.settings.lock().unwrap().projects.clone();
    let results = app
        .engines
        .sessions_for_projects(&app, &projects, active_project.as_deref())
        .await;
    results
        .into_iter()
        .map(|(project, res)| match res {
            Ok(sessions) => ProjectSessions { project, sessions, error: None },
            Err(error) => ProjectSessions {
                project,
                sessions: serde_json::Value::Array(Vec::new()),
                error: Some(error),
            },
        })
        .collect()
}

#[derive(Serialize)]
pub struct Opened {
    session: String,
    busy: bool,
}

/// Opens a saved session (replayed as events) or, without `session`, a new one.
#[tauri::command]
pub async fn acp_open(app: State<'_, Arc<App>>, project: String, session: Option<String>) -> Result<Opened> {
    let project = self::project(&app, &project)?;
    let engine = app.engines.get(&app, &project).await.map_err(|e| format!("{e:#}"))?;
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
pub async fn acp_prompt(
    app: State<'_, Arc<App>>,
    project: String,
    session: String,
    text: String,
    model: String,
    images: Option<Vec<superviser_backend::ImageAttachment>>,
) -> Result<()> {
    let (model, _) = app.loaded(&model).ok_or("Load the chat's model first.")?;
    app.touch(&model.id);
    app.sync_upstreams().await;
    let project = self::project(&app, &project)?;
    let engine = app.engines.get(&app, &project).await.map_err(|e| format!("{e:#}"))?;
    let imgs = images.as_deref().unwrap_or(&[]);
    let path_note = app
        .settings
        .lock()
        .unwrap()
        .include_image_paths
        .then(|| superviser_backend::attachments::format_image_paths_note(imgs, Some(&project)))
        .flatten();
    engine
        .prompt(app.bus.clone(), &session, &text, imgs, path_note, &model.alias)
        .map_err(|e| format!("{e:#}"))
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
pub async fn acp_delete(app: State<'_, Arc<App>>, project: String, session: String) -> Result<()> {
    let project = self::project(&app, &project)?;
    let engine = app.engines.get(&app, &project).await.map_err(|e| format!("{e:#}"))?;
    engine.delete_session(&session).await.map_err(|e| format!("{e:#}"))?;
    app.forget_chat(&project.id, &session).map_err(err)
}

/// A project's archived chats, of either engine.
#[tauri::command]
pub fn archived_chats(app: State<'_, Arc<App>>, project: String) -> Vec<String> {
    app.archived_chats(&project)
}

#[tauri::command]
pub fn set_chat_archived(app: State<'_, Arc<App>>, project: String, chat: String, archived: bool) -> Result<()> {
    app.set_chat_archived(&project, &chat, archived).map_err(err)
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

/// A project file the chat mentions, if it exists in the project's folders.
#[tauri::command]
pub fn project_file(app: State<'_, Arc<App>>, project: String, path: String) -> Option<FileInfo> {
    files::project_file(&app, &project, &path)
}

/// The editors installed, for "Open with".
#[tauri::command]
pub fn editors() -> Vec<&'static str> {
    files::editors()
}

/// Opens a project file in its default app (`how` = `default`), in one of `EDITORS`
/// (`editor`, with `editor` set), or shows it in Finder (`finder`).
#[tauri::command]
pub fn open_file(app: State<'_, Arc<App>>, project: String, path: String, how: String, editor: Option<String>) -> Result<()> {
    files::open_file(&app, &project, &path, &how, editor.as_deref()).map_err(err)
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
pub async fn chat_send(
    app: State<'_, Arc<App>>,
    project: String,
    chat: Option<String>,
    text: String,
    model: String,
    images: Option<Vec<superviser_backend::ImageAttachment>>,
) -> Result<String> {
    superviser_backend::agent::send(
        app.inner().clone(),
        &project,
        chat.as_deref(),
        &text,
        images.as_deref().unwrap_or(&[]),
        &model,
    )
    .map_err(|e| format!("{e:#}"))
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
    app.chats.stop_and_delete(&id);
    Chat::delete(&app.paths, &project, &id).map_err(err)?;
    app.forget_chat(&project, &id).map_err(err)
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

/// Ends the setup guide: marks setup done, opens the chat on `project` (starting `model`
/// meanwhile, so the first message doesn't wait for it) or Settings, and closes the guide.
#[tauri::command]
pub fn leave_setup(handle: AppHandle, app: State<'_, Arc<App>>, window: tauri::WebviewWindow, to: String, project: Option<String>, model: Option<String>) -> Result<()> {
    app.settings.lock().unwrap().setup_done = true;
    app.save_settings().map_err(err)?;
    crate::refresh_tray(&handle);
    // Started before the chat opens, so the chat finds it loading rather than loading it too.
    if let Some(model) = model.filter(|m| to == "chat" && !app.loading_or_loaded(m)) {
        let (handle, app) = (handle.clone(), app.inner().clone());
        tauri::async_runtime::spawn(async move {
            if let Err(e) = crate::load_model(app.clone(), model.clone()).await {
                // The chat may have started it meanwhile; only a real failure is worth a dialog.
                if !app.loading_or_loaded(&model) {
                    crate::show_error(&handle, "Couldn't start the model", &e);
                }
            }
        });
    }
    match to.as_str() {
        "chat" => crate::open_chat(&handle, project.as_deref()).map_err(err)?,
        "settings" => crate::open_window(&handle, "settings").map_err(err)?,
        _ => return Err(format!("no window called {to}")),
    }
    if window.label() == "setup" {
        window.close().map_err(err)?;
    }
    Ok(())
}

#[tauri::command]
pub fn open_window(handle: AppHandle, name: String) -> Result<()> {
    if !matches!(name.as_str(), "setup" | "settings" | "chat") {
        return Err(format!("no window called {name}"));
    }
    crate::open_window(&handle, &name).map_err(err)
}

// --- Host-side Git & GitHub PR actions (outside the agent's sandbox) ---

#[tauri::command]
pub async fn git_status(app: State<'_, Arc<App>>, project: String, check_pr: Option<bool>) -> Result<GitStatus> {
    let proj = self::project(&app, &project)?;
    let root = proj.main().to_path_buf();
    let mut st = git::status(&root).await;
    if check_pr.unwrap_or(false) && st.is_repo && st.has_remote && st.gh_available && !st.detached {
        st.open_pr = git::current_pr(&root).await;
    }
    Ok(st)
}

#[tauri::command]
pub async fn git_commit(app: State<'_, Arc<App>>, project: String, message: String, stage_all: Option<bool>) -> Result<CommitResult> {
    let proj = self::project(&app, &project)?;
    git::commit(proj.main(), &message, stage_all.unwrap_or(true)).await.map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn git_push(app: State<'_, Arc<App>>, project: String) -> Result<PushResult> {
    let proj = self::project(&app, &project)?;
    git::push(proj.main()).await.map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn git_create_pr(app: State<'_, Arc<App>>, project: String, title: String, body: String, draft: Option<bool>) -> Result<PullRequestInfo> {
    let proj = self::project(&app, &project)?;
    git::create_pr(proj.main(), &title, &body, draft.unwrap_or(false)).await.map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn git_generate_commit_message(app: State<'_, Arc<App>>, project: String, model: Option<String>) -> Result<String> {
    let proj = self::project(&app, &project)?;
    git::generate_commit_message(&app, proj.main(), model.as_deref()).await.map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn git_generate_pr_draft(app: State<'_, Arc<App>>, project: String, model: Option<String>) -> Result<PrDraft> {
    let proj = self::project(&app, &project)?;
    git::generate_pr_draft(&app, proj.main(), model.as_deref()).await.map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub fn open_external_url(url: String) -> Result<()> {
    files::open_url(&url).map_err(err)
}

// --- First-class Sub-Agent tracking ---

#[tauri::command]
pub fn subagents_list(app: State<'_, Arc<App>>, project: String, session: String) -> Vec<SubagentRecord> {
    app.subagents.for_session(&project, &session)
}

#[tauri::command]
pub fn subagent_cancel(app: State<'_, Arc<App>>, id: String) -> bool {
    app.subagents.cancel(&id, &app.bus)
}
