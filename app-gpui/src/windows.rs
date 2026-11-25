//! Window manager, macOS Dock ActivationPolicy toggling, and shared GPUI global state.

use std::sync::Arc;

use gpui_kit::{
    ActivationPolicy, AnyWindowHandle, App, AppContext, Bounds, Entity, Global, PathPromptOptions,
    WindowBounds, WindowOptions, px, size,
};

use crate::backend::Backend;
use crate::chat::ChatWindow;
use crate::settings::SettingsWindow;
use crate::setup::SetupWindow;
use crate::theme::apply_theme;
use crate::tray::TrayController;

pub struct AppGlobals {
    pub backend: Arc<Backend>,
    pub tray: Option<TrayController>,
    pub chat: Option<(AnyWindowHandle, Entity<ChatWindow>)>,
    pub settings: Option<(AnyWindowHandle, Entity<SettingsWindow>)>,
    pub setup: Option<(AnyWindowHandle, Entity<SetupWindow>)>,
}

impl Global for AppGlobals {}

impl AppGlobals {
    pub fn refresh_tray(&self) {
        if let Some(tray) = &self.tray {
            tray.refresh(&self.backend);
        }
    }
}

/// Shows the app in the macOS Dock + Cmd-Tab while any window is open, or hides it back
/// to the menu bar (`Accessory`) when all windows are closed.
pub fn sync_dock_policy(cx: &mut App) {
    let has_windows = !cx.windows().is_empty();
    if has_windows {
        cx.set_activation_policy(ActivationPolicy::Regular);
        cx.activate(true);
    } else {
        cx.set_activation_policy(ActivationPolicy::Accessory);
    }
}

fn centered_options(title: &str, width: f32, height: f32, cx: &App) -> WindowOptions {
    let bounds = Bounds::centered(None, size(px(width), px(height)), cx);
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(gpui_kit::TitlebarOptions {
            title: Some(title.to_string().into()),
            ..Default::default()
        }),
        window_min_size: Some(size(px(560.0), px(420.0))),
        focus: true,
        show: true,
        ..Default::default()
    }
}

pub fn open_chat(project_id: Option<String>, cx: &mut App) {
    cx.set_activation_policy(ActivationPolicy::Regular);
    cx.activate(true);

    if let Some((handle, entity)) = cx.global::<AppGlobals>().chat.clone() {
        let ok = handle.update(cx, |_, window, cx| {
            if let Some(pid) = project_id.clone() {
                entity.update(cx, |view, cx| view.select_project(pid, window, cx));
            }
            window.activate_window();
        });
        if ok.is_ok() {
            return;
        }
    }

    let backend = cx.global::<AppGlobals>().backend.clone();
    let options = centered_options("Agent Superviser Chat (GPUI Prototype)", 1040.0, 720.0, cx);
    match gpui_kit::open_window(options, cx, |window, cx| {
        apply_theme(Some(window), cx);
        cx.new(|cx| ChatWindow::new(backend, project_id, window, cx))
    }) {
        Ok((handle, entity)) => {
            cx.global_mut::<AppGlobals>().chat = Some((handle, entity));
        }
        Err(e) => tracing::warn!("opening chat window: {e:#}"),
    }
}

pub fn open_settings(initial_tab: Option<usize>, cx: &mut App) {
    cx.set_activation_policy(ActivationPolicy::Regular);
    cx.activate(true);

    if let Some((handle, entity)) = cx.global::<AppGlobals>().settings.clone() {
        let ok = handle.update(cx, |_, window, cx| {
            if let Some(tab) = initial_tab {
                entity.update(cx, |view, cx| view.set_tab(tab, cx));
            }
            window.activate_window();
        });
        if ok.is_ok() {
            return;
        }
    }

    let backend = cx.global::<AppGlobals>().backend.clone();
    let options = centered_options("Agent Superviser Settings (GPUI Prototype)", 900.0, 660.0, cx);
    match gpui_kit::open_window(options, cx, |window, cx| {
        apply_theme(Some(window), cx);
        cx.new(|cx| SettingsWindow::new(backend, initial_tab.unwrap_or(0), window, cx))
    }) {
        Ok((handle, entity)) => {
            cx.global_mut::<AppGlobals>().settings = Some((handle, entity));
        }
        Err(e) => tracing::warn!("opening settings window: {e:#}"),
    }
}

pub fn open_setup(cx: &mut App) {
    cx.set_activation_policy(ActivationPolicy::Regular);
    cx.activate(true);

    if let Some((handle, _)) = cx.global::<AppGlobals>().setup.clone()
        && handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        return;
    }

    let backend = cx.global::<AppGlobals>().backend.clone();
    let options = centered_options("Set Up Agent Superviser (GPUI Prototype)", 800.0, 680.0, cx);
    match gpui_kit::open_window(options, cx, |window, cx| {
        apply_theme(Some(window), cx);
        cx.new(|cx| SetupWindow::new(backend, window, cx))
    }) {
        Ok((handle, entity)) => {
            cx.global_mut::<AppGlobals>().setup = Some((handle, entity));
        }
        Err(e) => tracing::warn!("opening setup window: {e:#}"),
    }
}

/// Prompts for a macOS folder natively via GPUI and adds it to the prototype project list.
pub fn choose_project_folder(
    prompt_text: &'static str,
    open_in_chat: bool,
    cx: &mut App,
) {
    let rx = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some(prompt_text.into()),
    });
    cx.spawn(async move |cx| {
        let Ok(Ok(Some(paths))) = rx.await else {
            return;
        };
        let Some(folder) = paths.into_iter().next() else {
            return;
        };
        cx.update(|cx| {
            let project = cx
                .global::<AppGlobals>()
                .backend
                .add_prototype_project(&folder);
            cx.global::<AppGlobals>().refresh_tray();
            if open_in_chat {
                open_chat(Some(project.id), cx);
            }
        });
    })
    .detach();
}
