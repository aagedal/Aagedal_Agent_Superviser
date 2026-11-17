//! Agent Superviser: a menu bar app that runs a local model for coding agents, behind a
//! guard that checks every tool call the model makes.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod state;

use std::sync::Arc;

use superviser_guard::{CallEvent, Verdict};
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder};

use crate::state::{App, ServerStatus};

const TRAY_ID: &str = "main";

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let app = Arc::new(App::new().expect("app state"));
    let builder = tauri::Builder::default()
        .manage(app.clone())
        .setup(move |tauri_app| {
            #[cfg(target_os = "macos")]
            tauri_app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let handle = tauri_app.handle().clone();
            let event_handle = handle.clone();
            let event_app = app.clone();
            let on_event = Arc::new(move |event: &CallEvent| {
                event_app.record_event(event);
                let _ = event_handle.emit("guard-call", event);
                if event.verdict != Verdict::Passed {
                    refresh_tray(&event_handle);
                }
            });
            tauri::async_runtime::block_on(app.start_guard(on_event))?;
            app.rescan();

            TrayIconBuilder::with_id(TRAY_ID)
                .icon(Image::from_bytes(include_bytes!("../icons/tray.png"))?)
                .icon_as_template(true)
                .tooltip("Agent Superviser")
                .menu(&tray_menu(&handle, &app)?)
                .show_menu_on_left_click(true)
                .on_menu_event(on_menu_event)
                .build(tauri_app)?;

            // First run (no model ever started): open the model picker, which also shows
            // models found from other apps.
            if app.settings.lock().unwrap().last_model.is_none() {
                open_window(&handle, "settings")?;
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
            commands::delete_model,
            commands::start_model,
            commands::stop_model,
            commands::api_info,
            commands::regenerate_api_key,
            commands::set_hf_token,
            commands::setup_opencode,
            commands::recent_calls,
            commands::open_window,
        ]);

    builder.build(tauri::generate_context!()).expect("building the app").run(|handle, event| match event {
        // Closing the last window keeps the menu bar app running.
        RunEvent::ExitRequested { api, code: None, .. } => api.prevent_exit(),
        RunEvent::Exit => handle.state::<Arc<App>>().shutdown(),
        _ => {}
    });
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
    let status = app.status.lock().unwrap().clone();
    let running_id = match &status {
        ServerStatus::Running { model, .. } | ServerStatus::Loading { model, .. } => Some(model.clone()),
        _ => None,
    };
    let blocked = app.events.lock().unwrap().iter().filter(|e| e.verdict != Verdict::Passed).count();
    let guard_line = match app.guard_url() {
        Some(url) => format!("Guard on at {url} · {blocked} blocked"),
        None => "Guard is not running".into(),
    };

    let status_item = MenuItem::with_id(handle, "status", status_line(&status), false, None::<&str>)?;
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
            CheckMenuItem::with_id(handle, format!("start:{}", m.id), label, fits, running_id.as_deref() == Some(&m.id), None::<&str>)
        })
        .collect::<tauri::Result<_>>()?;
    let none = MenuItem::with_id(handle, "none", "No models yet. Open Settings to download one.", false, None::<&str>)?;
    let model_refs: Vec<&dyn IsMenuItem<tauri::Wry>> = if model_items.is_empty() {
        vec![&none]
    } else {
        model_items.iter().map(|i| i as &dyn IsMenuItem<tauri::Wry>).collect()
    };
    let models = Submenu::with_id_and_items(handle, "models", "Start Model", true, &model_refs)?;
    let stop = MenuItem::with_id(handle, "stop", "Stop Model", running_id.is_some(), None::<&str>)?;
    let chat = MenuItem::with_id(handle, "chat", "Open Chat…", matches!(status, ServerStatus::Running { .. }), None::<&str>)?;
    let opencode = MenuItem::with_id(handle, "opencode", "Set Up OpenCode", matches!(status, ServerStatus::Running { .. }), None::<&str>)?;
    let settings = MenuItem::with_id(handle, "settings", "Settings…", true, Some("CmdOrCtrl+,"))?;
    let quit = MenuItem::with_id(handle, "quit", "Quit Agent Superviser", true, Some("CmdOrCtrl+Q"))?;
    Menu::with_items(
        handle,
        &[
            &status_item,
            &guard_item,
            &PredefinedMenuItem::separator(handle)?,
            &models,
            &stop,
            &PredefinedMenuItem::separator(handle)?,
            &chat,
            &opencode,
            &settings,
            &PredefinedMenuItem::separator(handle)?,
            &quit,
        ],
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
    let status = handle.state::<Arc<App>>().status.lock().unwrap().clone();
    let _ = handle.emit("status", &status);
    refresh_tray(handle);
}

pub async fn start_model(handle: AppHandle, app: Arc<App>, id: String) -> Result<(), String> {
    let status_handle = handle.clone();
    let result = app
        .start_model(&id, |status| {
            let _ = status_handle.emit("status", status);
            refresh_tray(&status_handle);
        })
        .await;
    status_changed(&handle);
    result.map_err(|e| format!("{e:#}"))
}

fn on_menu_event(handle: &AppHandle, event: tauri::menu::MenuEvent) {
    let id = event.id().as_ref().to_string();
    let app = handle.state::<Arc<App>>().inner().clone();
    let handle = handle.clone();
    match id.as_str() {
        "quit" => handle.exit(0),
        "settings" | "chat" => {
            if let Err(e) = open_window(&handle, &id) {
                tracing::warn!("opening {id}: {e}");
            }
        }
        "stop" => {
            tauri::async_runtime::spawn(async move {
                app.stop_model().await;
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
            if let Some(model) = id.strip_prefix("start:") {
                let model = model.to_string();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = start_model(handle, app, model).await {
                        tracing::warn!("starting model: {e}");
                    }
                });
            }
        }
    }
}

/// Shows the `settings` or `chat` window, creating it if needed.
pub fn open_window(handle: &AppHandle, name: &str) -> tauri::Result<()> {
    open_window_at(handle, name, "")
}

/// Like [`open_window`], on a tab of the settings window.
pub fn open_window_at(handle: &AppHandle, name: &str, tab: &str) -> tauri::Result<()> {
    if let Some(window) = handle.get_webview_window(name) {
        if !tab.is_empty() {
            window.eval(format!("showTab({tab:?})"))?;
        }
        window.show()?;
        return window.set_focus();
    }
    let (title, size) = match name {
        "chat" => ("Chat", (720.0, 640.0)),
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
