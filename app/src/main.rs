//! Agent Superviser: a menu bar app that runs a local model for coding agents, behind a
//! guard that checks every tool call the model makes.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod acp;
mod agent;
mod commands;
mod state;
mod update;

use std::sync::Arc;

use superviser_core::sandbox::Isolation;
use superviser_guard::{CallEvent, Verdict};
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

use crate::state::{App, LoadState, ServerStatus};

const TRAY_ID: &str = "main";
/// Projects listed in the menu bar menu; the rest are in Settings.
const MAX_TRAY_PROJECTS: usize = 8;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let app = Arc::new(App::new().expect("app state"));
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(app.clone())
        .manage(update::Updates::default())
        // In the Dock and Command-Tab only while one of its windows is open.
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::Destroyed = event {
                let label = window.label().to_string();
                let handle = window.app_handle();
                if handle.webview_windows().keys().all(|l| *l == label) {
                    set_in_dock(handle, false);
                }
            }
        })
        .setup(move |tauri_app| {
            #[cfg(target_os = "macos")]
            tauri_app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let handle = tauri_app.handle().clone();
            let event_handle = handle.clone();
            let event_app = app.clone();
            let on_event = Arc::new(move |event: &CallEvent| {
                event_app.record_event(event);
                let _ = event_handle.emit("guard-call", event);
                if matches!(event.verdict, Verdict::Blocked | Verdict::Flagged) {
                    refresh_tray(&event_handle);
                }
            });
            tauri::async_runtime::block_on(app.start_guard(on_event))?;

            // Killed (Activity Monitor, `kill`, a logout): quit properly, stopping the model servers.
            #[cfg(unix)]
            {
                let quit = handle.clone();
                tauri::async_runtime::spawn(async move {
                    use tokio::signal::unix::{SignalKind, signal};
                    let (Ok(mut term), Ok(mut interrupt)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else { return };
                    tokio::select! {
                        _ = term.recv() => {}
                        _ = interrupt.recv() => {}
                    }
                    quit.exit(0);
                });
            }
            app.rescan();

            TrayIconBuilder::with_id(TRAY_ID)
                .icon(Image::from_bytes(include_bytes!("../icons/tray.png"))?)
                .icon_as_template(true)
                .tooltip("Agent Superviser")
                .menu(&tray_menu(&handle, &app)?)
                .show_menu_on_left_click(true)
                .on_menu_event(on_menu_event)
                .build(tauri_app)?;

            update::start(&handle);

            // Until the first-run guide is finished or skipped, open it.
            if needs_setup(&app) {
                open_window(&handle, "setup")?;
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::overview,
            commands::catalog_offers,
            commands::search_models,
            commands::check_model,
            commands::download_model,
            commands::cancel_download,
            commands::installed_models,
            commands::other_apps,
            commands::rescan_models,
            commands::set_use_other_apps,
            commands::saved_conversations,
            commands::set_save_conversations,
            commands::clear_saved_conversations,
            commands::delete_model,
            commands::load_model,
            commands::unload_model,
            commands::room_for,
            commands::api_info,
            commands::regenerate_api_key,
            commands::set_hf_token,
            commands::setup_opencode,
            commands::recent_calls,
            commands::judge_settings,
            commands::set_judge,
            commands::download_judge,
            commands::open_window,
            commands::projects_info,
            commands::choose_folder,
            commands::new_project,
            commands::save_project,
            commands::delete_project,
            commands::open_project,
            commands::project_file,
            commands::editors,
            commands::open_file,
            commands::chat_engine,
            commands::chat_model,
            commands::set_chat_model,
            commands::acp_sessions,
            commands::acp_open,
            commands::acp_prompt,
            commands::acp_cancel,
            commands::acp_permission,
            commands::acp_delete,
            commands::archived_chats,
            commands::set_chat_archived,
            commands::chat_list,
            commands::chat_load,
            commands::chat_send,
            commands::chat_stop,
            commands::chat_answer,
            commands::chat_delete,
            commands::open_chat,
            commands::setup_done,
            commands::set_setup_done,
            commands::leave_setup,
            commands::get_installer,
        ]);

    builder.build(tauri::generate_context!()).expect("building the app").run(|handle, event| match event {
        // Closing the last window keeps the menu bar app running.
        RunEvent::ExitRequested { api, code: None, .. } => api.prevent_exit(),
        RunEvent::Exit => handle.state::<Arc<App>>().shutdown(),
        _ => {}
    });
}

/// Whether to offer the setup guide: not finished or skipped, and not set up some other way
/// (a model that fits on this Mac and a project, from Settings).
fn needs_setup(app: &App) -> bool {
    let (done, projects) = {
        let settings = app.settings.lock().unwrap();
        (settings.setup_done, !settings.projects.is_empty())
    };
    !done && !(projects && app.installed().iter().any(|m| app.fit(&m.model).is_some()))
}

fn status_line(status: &ServerStatus) -> String {
    match status {
        ServerStatus::Stopped => "No model running".into(),
        ServerStatus::Loading { name, .. } => format!("Loading {name}…"),
        ServerStatus::Running { name, context, .. } => format!("● {name} ({}k context)", context / 1024),
        ServerStatus::Failed { error, .. } => format!("Failed: {}", error.chars().take(60).collect::<String>()),
    }
}

fn tray_menu(handle: &AppHandle, app: &App) -> tauri::Result<Menu<tauri::Wry>> {
    let status = app.status();
    let loaded = app.models();
    let running = loaded.iter().filter(|m| matches!(m.state, LoadState::Running { .. })).count();
    let blocked = app.events.lock().unwrap().iter().filter(|e| matches!(e.verdict, Verdict::Blocked | Verdict::Flagged)).count();
    let guard_line = match app.guard_url() {
        Some(url) => format!("Guard on at {url} · {blocked} blocked"),
        None => "Guard is not running".into(),
    };

    let line = if running > 1 { format!("● {running} models loaded") } else { status_line(&status) };
    let status_item = MenuItem::with_id(handle, "status", line, false, None::<&str>)?;
    let guard_item = MenuItem::with_id(handle, "guard", guard_line, false, None::<&str>)?;

    let installed = app.installed();
    let model_items: Vec<CheckMenuItem<tauri::Wry>> = installed
        .iter()
        .map(|m| {
            let label = match m.source {
                superviser_core::installed::Source::App => format!("{} · {}", m.model.name, m.model.quant.label),
                other => format!("{} · {} ({})", m.model.name, m.model.quant.label, other.label()),
            };
            let fits = app.fit(&m.model).is_some();
            let on = loaded.iter().any(|l| l.id == m.id && !matches!(l.state, LoadState::Failed { .. }));
            CheckMenuItem::with_id(handle, format!("load:{}", m.id), label, fits, on, None::<&str>)
        })
        .collect::<tauri::Result<_>>()?;
    let none = MenuItem::with_id(handle, "none", "No models yet. Open Settings to download one.", false, None::<&str>)?;
    let model_refs: Vec<&dyn IsMenuItem<tauri::Wry>> = if model_items.is_empty() {
        vec![&none]
    } else {
        model_items.iter().map(|i| i as &dyn IsMenuItem<tauri::Wry>).collect()
    };
    let models = Submenu::with_id_and_items(handle, "models", "Load Model", true, &model_refs)?;
    let stop = MenuItem::with_id(handle, "stop", if running > 1 { "Unload All Models" } else { "Unload Model" }, running > 0, None::<&str>)?;
    let chat = MenuItem::with_id(handle, "chat", "Open Chat…", true, None::<&str>)?;
    let opencode = MenuItem::with_id(handle, "opencode", "Set Up OpenCode", matches!(status, ServerStatus::Running { .. }), None::<&str>)?;
    let running = matches!(status, ServerStatus::Running { .. });
    let projects = app.settings.lock().unwrap().projects.clone();
    // The chat; it runs Linux VM projects with OpenCode, so without it they open in Terminal.
    let chat_in_vm = superviser_core::opencode::binary().is_some();
    let chat_items: Vec<MenuItem<tauri::Wry>> = projects
        .iter()
        .take(MAX_TRAY_PROJECTS)
        .map(|p| match p.isolation {
            Isolation::LinuxVm if !chat_in_vm => MenuItem::with_id(handle, format!("project:{}", p.id), format!("{} (OpenCode, Linux VM)", p.name), running, None::<&str>),
            _ => MenuItem::with_id(handle, format!("chat:{}", p.id), &p.name, true, None::<&str>),
        })
        .collect::<tauri::Result<_>>()?;
    let opencode_items: Vec<MenuItem<tauri::Wry>> = projects
        .iter()
        .take(MAX_TRAY_PROJECTS)
        .map(|p| MenuItem::with_id(handle, format!("project:{}", p.id), &p.name, running, None::<&str>))
        .collect::<tauri::Result<_>>()?;
    let choose = MenuItem::with_id(handle, "choose-project", "New Project from Folder…", true, Some("CmdOrCtrl+O"))?;
    let separator = PredefinedMenuItem::separator(handle)?;
    let mut chat_refs: Vec<&dyn IsMenuItem<tauri::Wry>> = chat_items.iter().map(|i| i as &dyn IsMenuItem<tauri::Wry>).collect();
    if !chat_refs.is_empty() {
        chat_refs.push(&separator);
    }
    chat_refs.push(&choose);
    let code = Submenu::with_id_and_items(handle, "code", "Code in a Project", cfg!(target_os = "macos"), &chat_refs)?;
    let opencode_refs: Vec<&dyn IsMenuItem<tauri::Wry>> = opencode_items.iter().map(|i| i as &dyn IsMenuItem<tauri::Wry>).collect();
    let in_opencode = Submenu::with_id_and_items(handle, "in-opencode", "Open in OpenCode", cfg!(target_os = "macos") && !opencode_refs.is_empty(), &opencode_refs)?;
    let settings = MenuItem::with_id(handle, "settings", "Settings…", true, Some("CmdOrCtrl+,"))?;
    // Until setup is finished or skipped, a way back to it.
    let setup = needs_setup(app).then(|| MenuItem::with_id(handle, "setup", "Continue Setup…", true, None::<&str>)).transpose()?;
    let quit = MenuItem::with_id(handle, "quit", "Quit Agent Superviser", true, Some("CmdOrCtrl+Q"))?;
    let update = handle.state::<update::Updates>().available().map(|(version, installing)| {
        let label = if installing { format!("Installing {version}…") } else { format!("Update to {version} and Restart…") };
        MenuItem::with_id(handle, "update", label, !installing, None::<&str>)
    });
    let update = update.transpose()?;
    let separator_top = PredefinedMenuItem::separator(handle)?;
    let mut top: Vec<&dyn IsMenuItem<tauri::Wry>> = Vec::new();
    if let Some(update) = &update {
        top.extend([update as &dyn IsMenuItem<tauri::Wry>, &separator_top]);
    }
    let separator_setup = PredefinedMenuItem::separator(handle)?;
    if let Some(setup) = &setup {
        top.extend([setup as &dyn IsMenuItem<tauri::Wry>, &separator_setup]);
    }
    Menu::with_items(
        handle,
        &[
            &top[..],
            &[
            &status_item,
            &guard_item,
            &PredefinedMenuItem::separator(handle)?,
            &models,
            &stop,
            &PredefinedMenuItem::separator(handle)?,
            &code,
            &chat,
            &in_opencode,
            &opencode,
            &settings,
            &PredefinedMenuItem::separator(handle)?,
            &quit,
            ],
        ]
        .concat(),
    )
}

/// Rebuilds the tray menu from the current state.
pub fn refresh_tray(handle: &AppHandle) {
    let app = handle.state::<Arc<App>>();
    if let Some(tray) = handle.tray_by_id(TRAY_ID) {
        match tray_menu(handle, &app) {
            Ok(menu) => {
                let _ = tray.set_menu(Some(menu));
            }
            Err(e) => tracing::warn!("tray menu: {e}"),
        }
    }
}

/// Tells windows and the tray that the server status changed.
pub fn status_changed(handle: &AppHandle) {
    let status = handle.state::<Arc<App>>().status();
    let _ = handle.emit("status", &status);
    refresh_tray(handle);
}

pub async fn load_model(handle: AppHandle, app: Arc<App>, id: String) -> Result<(), String> {
    let status_handle = handle.clone();
    let result = app.load_model(&id, || status_changed(&status_handle)).await;
    status_changed(&handle);
    result.map_err(|e| format!("{e:#}"))
}

fn on_menu_event(handle: &AppHandle, event: tauri::menu::MenuEvent) {
    let id = event.id().as_ref().to_string();
    let app = handle.state::<Arc<App>>().inner().clone();
    let handle = handle.clone();
    match id.as_str() {
        "quit" => handle.exit(0),
        "update" => update::install(&handle),
        "chat" => {
            if let Err(e) = open_chat(&handle, None) {
                tracing::warn!("opening chat: {e}");
            }
        }
        "settings" | "setup" => {
            if let Err(e) = open_window(&handle, &id) {
                tracing::warn!("opening {id}: {e}");
            }
        }
        "choose-project" => choose_project(&handle),
        "stop" => {
            tauri::async_runtime::spawn(async move {
                app.unload_model(None).await;
                status_changed(&handle);
            });
        }
        // The Connect tab runs the setup and shows what happened.
        "opencode" => {
            if let Err(e) = open_window_at(&handle, "settings", "connect") {
                tracing::warn!("opening settings: {e}");
            }
        }
        _ => {
            if let Some(project) = id.strip_prefix("chat:") {
                if let Err(e) = open_chat(&handle, Some(project)) {
                    tracing::warn!("opening chat: {e}");
                }
            } else if let Some(project) = id.strip_prefix("project:") {
                open_project(&handle, project);
            } else if let Some(model) = id.strip_prefix("load:") {
                // A loaded model's item unloads it.
                let model = model.to_string();
                let loaded = app.models().iter().any(|m| m.id == model && !matches!(m.state, LoadState::Failed { .. }));
                tauri::async_runtime::spawn(async move {
                    if loaded {
                        app.unload_model(Some(&model)).await;
                        status_changed(&handle);
                    } else if let Err(e) = load_model(handle.clone(), app, model).await {
                        show_error(&handle, "Couldn't load the model", &e);
                    }
                });
            }
        }
    }
}

/// Opens an https link in the default browser.
pub fn open_url(url: &str) -> std::io::Result<()> {
    if !url.starts_with("https://") {
        return Err(std::io::Error::other("only https links"));
    }
    let opener = if cfg!(target_os = "macos") { "/usr/bin/open" } else if cfg!(windows) { "explorer" } else { "xdg-open" };
    std::process::Command::new(opener).arg(url).status().map(|_| ())
}

/// Asks for a folder, then opens its project (made if needed) in the chat window.
pub fn choose_project(handle: &AppHandle) {
    let handle_for_pick = handle.clone();
    let mut dialog = handle.dialog().file().set_title("Choose a project folder for OpenCode");
    if let Some(home) = std::env::var_os("HOME") {
        dialog = dialog.set_directory(home);
    }
    dialog.pick_folder(move |folder| {
        if let Some(path) = folder.and_then(|f| f.into_path().ok()) {
            let app = handle_for_pick.state::<Arc<App>>().inner().clone();
            match app.project_for_folder(&path) {
                Ok(project) => {
                    refresh_tray(&handle_for_pick);
                    if let Err(e) = open_chat(&handle_for_pick, Some(&project.id)) {
                        tracing::warn!("opening chat: {e}");
                    }
                }
                Err(e) => show_error(&handle_for_pick, "Couldn't make a project", &format!("{e:#}")),
            }
        }
    });
}

/// Opens a project in sandboxed OpenCode, saying why if it can't.
pub fn open_project(handle: &AppHandle, id: &str) {
    let app = handle.state::<Arc<App>>().inner().clone();
    match app.open_project(id) {
        Ok(_) => {
            let _ = handle.emit("projects", ());
            refresh_tray(handle);
        }
        Err(e) => show_error(handle, "Couldn't open the project", &format!("{e:#}")),
    }
}

pub fn show_error(handle: &AppHandle, title: &str, message: &str) {
    handle.dialog().message(message).title(title).kind(MessageDialogKind::Error).show(|_| {});
}

/// Shows the `setup`, `settings` or `chat` window, creating it if needed.
pub fn open_window(handle: &AppHandle, name: &str) -> tauri::Result<()> {
    open_window_at(handle, name, "")
}

/// Shows the chat window, on `project` when given (else on the most recent project).
pub fn open_chat(handle: &AppHandle, project: Option<&str>) -> tauri::Result<()> {
    open_window_at(handle, "chat", project.unwrap_or(""))
}

/// Shows the app in the Dock and Command-Tab (while it has a window open), or hides it to
/// the menu bar.
fn set_in_dock(handle: &AppHandle, shown: bool) {
    #[cfg(target_os = "macos")]
    {
        let policy = if shown { tauri::ActivationPolicy::Regular } else { tauri::ActivationPolicy::Accessory };
        if let Err(e) = handle.set_activation_policy(policy) {
            tracing::warn!("activation policy: {e}");
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (handle, shown);
}

/// Like [`open_window`], on a tab of the settings window or a project in the chat window.
pub fn open_window_at(handle: &AppHandle, name: &str, tab: &str) -> tauri::Result<()> {
    set_in_dock(handle, true);
    if let Some(window) = handle.get_webview_window(name) {
        if !tab.is_empty() {
            let call = if name == "chat" { "openProject" } else { "showTab" };
            let arg = serde_json::to_string(tab).unwrap_or_else(|_| "\"\"".into());
            window.eval(format!("{call}({arg})"))?;
        }
        window.show()?;
        return window.set_focus();
    }
    let (title, size) = match name {
        "chat" => ("Agent Superviser Chat", (1000.0, 720.0)),
        "setup" => ("Set Up Agent Superviser", (800.0, 680.0)),
        _ => ("Agent Superviser", (880.0, 640.0)),
    };
    let page = if tab.is_empty() { format!("{name}.html") } else { format!("{name}.html#{tab}") };
    WebviewWindowBuilder::new(handle, name, WebviewUrl::App(page.into()))
        .title(title)
        .inner_size(size.0, size.1)
        .min_inner_size(560.0, 420.0)
        .focused(true)
        .build()?
        .set_focus()
}
