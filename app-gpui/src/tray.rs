//! Native macOS menu bar tray icon (`tray-icon` + `muda`) wired into GPUI's run loop.

use std::sync::Arc;

use anyhow::{Context, Result};
use muda::{CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use superviser_core::installed::Source;
use tokio::sync::mpsc;
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::backend::Backend;

const MAX_TRAY_PROJECTS: usize = 8;

#[derive(Debug, Clone)]
pub enum TrayAction {
    OpenChat(Option<String>),
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
        let menu = build_menu(backend)?;
        let tray = TrayIconBuilder::new()
            .with_icon(icon)
            .with_icon_as_template(true)
            .with_tooltip("Agent Superviser (GPUI Prototype)")
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

    pub fn refresh(&self, backend: &Backend) {
        match build_menu(backend) {
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
        "connect" => Some(TrayAction::OpenSettings(Some(3))),
        "setup" => Some(TrayAction::OpenSetup),
        "choose-project" => Some(TrayAction::ChooseProjectFolder),
        "stop" => Some(TrayAction::UnloadAll),
        _ => {
            if let Some(proj) = id.strip_prefix("chat:") {
                Some(TrayAction::OpenChat(Some(proj.to_string())))
            } else {
                id.strip_prefix("load:")
                    .map(|model| TrayAction::ToggleModel(model.to_string()))
            }
        }
    }
}

fn build_menu(backend: &Backend) -> Result<Menu> {
    let active_id = backend.active_model_id();
    let installed = backend.installed();
    let active_name = active_id.as_deref().and_then(|id| {
        installed
            .iter()
            .find(|m| m.id == id)
            .map(|m| m.model.name.clone())
    });

    let status_text = match &active_name {
        Some(name) => format!("● {name} (prototype active)"),
        None => "No model running".into(),
    };
    let blocked = backend.blocked_count();
    let guard_text = match backend.guard_url() {
        Some(url) => format!("Guard on at {url} · {blocked} blocked"),
        None => "Guard is not running".into(),
    };

    let status_item = MenuItem::with_id("status", status_text, false, None);
    let guard_item = MenuItem::with_id("guard", guard_text, false, None);

    let model_items: Vec<CheckMenuItem> = installed
        .iter()
        .map(|m| {
            let label = match m.source {
                Source::App => format!("{} · {}", m.model.name, m.model.quant.label),
                other => format!("{} · {} ({})", m.model.name, m.model.quant.label, other.label()),
            };
            let fits = backend.fit(&m.model).is_some();
            let checked = active_id.as_deref() == Some(m.id.as_str());
            CheckMenuItem::with_id(format!("load:{}", m.id), label, fits, checked, None)
        })
        .collect();
    let none_item = MenuItem::with_id(
        "none",
        "No models yet. Open Settings to browse catalog.",
        false,
        None,
    );
    let model_refs: Vec<&dyn IsMenuItem> = if model_items.is_empty() {
        vec![&none_item]
    } else {
        model_items.iter().map(|i| i as &dyn IsMenuItem).collect()
    };
    let models_sub = Submenu::with_id_and_items("models", "Load Model", true, &model_refs)?;
    let stop_item = MenuItem::with_id("stop", "Unload Model", active_id.is_some(), None);

    let projects = backend.projects();
    let project_items: Vec<MenuItem> = projects
        .iter()
        .take(MAX_TRAY_PROJECTS)
        .map(|p| MenuItem::with_id(format!("chat:{}", p.id), &p.name, true, None))
        .collect();
    let sep_proj = PredefinedMenuItem::separator();
    let choose_item = MenuItem::with_id("choose-project", "New Project from Folder…", true, None);
    let mut proj_refs: Vec<&dyn IsMenuItem> = project_items
        .iter()
        .map(|i| i as &dyn IsMenuItem)
        .collect();
    if !proj_refs.is_empty() {
        proj_refs.push(&sep_proj);
    }
    proj_refs.push(&choose_item);
    let code_sub = Submenu::with_id_and_items("code", "Code in a Project", true, &proj_refs)?;

    let chat_item = MenuItem::with_id("chat", "Open Chat…", true, None);
    let connect_item = MenuItem::with_id("connect", "Connect OpenCode…", true, None);
    let settings_item = MenuItem::with_id("settings", "Settings…", true, None);
    let setup_item = MenuItem::with_id("setup", "Setup Guide…", true, None);
    let quit_item = MenuItem::with_id("quit", "Quit Agent Superviser", true, None);

    let menu = Menu::new();
    menu.append_items(&[
        &status_item,
        &guard_item,
        &PredefinedMenuItem::separator(),
        &models_sub,
        &stop_item,
        &PredefinedMenuItem::separator(),
        &code_sub,
        &chat_item,
        &connect_item,
        &settings_item,
        &setup_item,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ])?;
    Ok(menu)
}
