//! Native macOS menu bar tray icon (`tray-icon` + `muda`) wired into GPUI's run loop.

use std::sync::Arc;

use anyhow::{Context, Result};
use muda::{CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use superviser_backend::{App, LoadState, ServerStatus};
use superviser_core::installed::Source;
use superviser_core::sandbox::Isolation;
use superviser_guard::Verdict;
use tokio::sync::mpsc;
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::backend::Backend;

const MAX_TRAY_PROJECTS: usize = 8;

#[derive(Debug, Clone)]
pub enum TrayAction {
    OpenChat(Option<String>),
    OpenProjectInOpenCode(String),
    OpenSettings(Option<usize>),
    OpenSetup,
    ChooseProjectFolder,
    ToggleModel(String),
    UnloadAll,
    Quit,
}

pub struct TrayController {
    tray: TrayIcon,
}

impl TrayController {
    pub fn new(backend: &Arc<Backend>) -> Result<(Self, mpsc::UnboundedReceiver<TrayAction>)> {
        let icon = load_tray_icon()?;
        let menu = build_menu(&backend.app)?;
        let tray = TrayIconBuilder::new()
            .with_icon(icon)
            .with_icon_as_template(true)
            .with_tooltip("Agent Superviser")
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(true)
            .build()
            .context("building macOS tray icon")?;

        let (tx, rx) = mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name("tray-menu-events".into())
            .spawn(move || {
                let menu_rx = MenuEvent::receiver();
                while let Ok(event) = menu_rx.recv() {
                    if let Some(action) = parse_menu_id(event.id.0.as_str())
                        && tx.send(action).is_err()
                    {
                        break;
                    }
                }
            })?;

        Ok((Self { tray }, rx))
    }

    pub fn refresh(&self, app: &App) {
        match build_menu(app) {
            Ok(menu) => self.tray.set_menu(Some(Box::new(menu))),
            Err(e) => tracing::warn!("failed to rebuild tray menu: {e:#}"),
        }
    }
}

fn load_tray_icon() -> Result<Icon> {
    let bytes = include_bytes!("../../app/icons/tray.png");
    let img = image::load_from_memory(bytes)
        .context("decoding tray.png")?
        .into_rgba8();
    let (w, h) = img.dimensions();
    Icon::from_rgba(img.into_raw(), w, h).context("creating tray icon")
}

fn parse_menu_id(id: &str) -> Option<TrayAction> {
    match id {
        "quit" => Some(TrayAction::Quit),
        "chat" => Some(TrayAction::OpenChat(None)),
        "settings" => Some(TrayAction::OpenSettings(None)),
        "opencode" => Some(TrayAction::OpenSettings(Some(3))),
        "setup" => Some(TrayAction::OpenSetup),
        "choose-project" => Some(TrayAction::ChooseProjectFolder),
        "stop" => Some(TrayAction::UnloadAll),
        _ => {
            if let Some(proj) = id.strip_prefix("chat:") {
                Some(TrayAction::OpenChat(Some(proj.to_string())))
            } else if let Some(proj) = id.strip_prefix("project:") {
                Some(TrayAction::OpenProjectInOpenCode(proj.to_string()))
            } else {
                id.strip_prefix("load:")
                    .map(|model| TrayAction::ToggleModel(model.to_string()))
            }
        }
    }
}

fn status_line(status: &ServerStatus) -> String {
    match status {
        ServerStatus::Stopped => "No model running".into(),
        ServerStatus::Loading { name, .. } => format!("Loading {name}…"),
        ServerStatus::Running { name, context, .. } => {
            format!("● {name} ({}k context)", context / 1024)
        }
        ServerStatus::Failed { error, .. } => {
            format!("Failed: {}", error.chars().take(60).collect::<String>())
        }
    }
}

fn build_menu(app: &App) -> Result<Menu> {
    let status = app.status();
    let loaded = app.models();
    let running_count = loaded
        .iter()
        .filter(|m| matches!(m.state, LoadState::Running { .. }))
        .count();
    let any_active = loaded
        .iter()
        .any(|m| !matches!(m.state, LoadState::Failed { .. }));
    let blocked = app
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| matches!(e.verdict, Verdict::Blocked | Verdict::Flagged))
        .count();

    let status_text = if running_count > 1 {
        format!("● {running_count} models loaded")
    } else {
        status_line(&status)
    };
    let guard_text = match app.guard_url() {
        Some(url) => format!("Guard on at {url} · {blocked} blocked"),
        None => "Guard is not running".into(),
    };

    let status_item = MenuItem::with_id("status", status_text, false, None);
    let guard_item = MenuItem::with_id("guard", guard_text, false, None);

    let installed = app.installed();
    let model_items: Vec<CheckMenuItem> = installed
        .iter()
        .map(|m| {
            let label = match m.source {
                Source::App => format!("{} · {}", m.model.name, m.model.quant.label),
                other => format!(
                    "{} · {} ({})",
                    m.model.name,
                    m.model.quant.label,
                    other.label()
                ),
            };
            let fits = app.fit(&m.model).is_some();
            let on = loaded
                .iter()
                .any(|l| l.id == m.id && !matches!(l.state, LoadState::Failed { .. }));
            CheckMenuItem::with_id(format!("load:{}", m.id), label, fits || on, on, None)
        })
        .collect();
    let none_item = MenuItem::with_id(
        "none",
        "No models yet. Open Settings to download one.",
        false,
        None,
    );
    let model_refs: Vec<&dyn IsMenuItem> = if model_items.is_empty() {
        vec![&none_item]
    } else {
        model_items.iter().map(|i| i as &dyn IsMenuItem).collect()
    };
    let models_sub = Submenu::with_id_and_items("models", "Load Model", true, &model_refs)?;
    let stop_label = if running_count > 1 {
        "Unload All Models"
    } else {
        "Unload Model"
    };
    let stop_item = MenuItem::with_id("stop", stop_label, any_active, None);

    let is_running = matches!(status, ServerStatus::Running { .. });
    let projects = app.settings.lock().unwrap().projects.clone();
    let chat_in_vm = superviser_core::opencode::binary().is_some();

    let chat_items: Vec<MenuItem> = projects
        .iter()
        .take(MAX_TRAY_PROJECTS)
        .map(|p| match p.isolation {
            Isolation::LinuxVm if !chat_in_vm => MenuItem::with_id(
                format!("project:{}", p.id),
                format!("{} (OpenCode, Linux VM)", p.name),
                is_running,
                None,
            ),
            _ => MenuItem::with_id(format!("chat:{}", p.id), &p.name, true, None),
        })
        .collect();
    let sep_proj = PredefinedMenuItem::separator();
    let choose_item = MenuItem::with_id("choose-project", "New Project from Folder…", true, None);
    let mut proj_refs: Vec<&dyn IsMenuItem> = chat_items
        .iter()
        .map(|i| i as &dyn IsMenuItem)
        .collect();
    if !proj_refs.is_empty() {
        proj_refs.push(&sep_proj);
    }
    proj_refs.push(&choose_item);
    let code_sub = Submenu::with_id_and_items(
        "code",
        "Code in a Project",
        cfg!(target_os = "macos"),
        &proj_refs,
    )?;

    let opencode_items: Vec<MenuItem> = projects
        .iter()
        .take(MAX_TRAY_PROJECTS)
        .map(|p| MenuItem::with_id(format!("project:{}", p.id), &p.name, is_running, None))
        .collect();
    let opencode_refs: Vec<&dyn IsMenuItem> = opencode_items
        .iter()
        .map(|i| i as &dyn IsMenuItem)
        .collect();
    let in_opencode_sub = Submenu::with_id_and_items(
        "in-opencode",
        "Open in OpenCode",
        cfg!(target_os = "macos") && !opencode_refs.is_empty(),
        &opencode_refs,
    )?;

    let chat_item = MenuItem::with_id("chat", "Open Chat…", true, None);
    let opencode_item = MenuItem::with_id("opencode", "Set Up OpenCode", is_running, None);
    let settings_item = MenuItem::with_id("settings", "Settings…", true, None);
    let setup_item = app
        .needs_setup()
        .then(|| MenuItem::with_id("setup", "Continue Setup…", true, None));
    let quit_item = MenuItem::with_id("quit", "Quit Agent Superviser", true, None);

    let menu = Menu::new();
    if let Some(setup) = &setup_item {
        menu.append_items(&[setup as &dyn IsMenuItem, &PredefinedMenuItem::separator()])?;
    }
    menu.append_items(&[
        &status_item,
        &guard_item,
        &PredefinedMenuItem::separator(),
        &models_sub,
        &stop_item,
        &PredefinedMenuItem::separator(),
        &code_sub,
        &chat_item,
        &in_opencode_sub,
        &opencode_item,
        &settings_item,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ])?;
    Ok(menu)
}
