//! Window manager, macOS Dock ActivationPolicy toggling, and shared GPUI global state.

use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::{
    ActivationPolicy, AnyWindowHandle, App, AppContext, Bounds, Entity, Global, PathPromptOptions,
    WindowBounds, WindowId, WindowOptions, px, size,
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
            tray.refresh(&self.backend.app);
        }
    }
}

/// Sets the macOS Dock tile and app-switcher (`⌘Tab`) icon from the bundled `app/icons/icon.png`
/// so the icon appears both inside `.app` bundles and when launched directly via `cargo run` or
/// a standalone binary.
#[cfg(target_os = "macos")]
pub fn set_app_icon() {
    use objc2::AnyThread;
    use objc2_app_kit::{NSApplication, NSImage, NSImageScaling, NSImageView};
    use objc2_foundation::{MainThreadMarker, NSData, NSPoint, NSRect, NSSize};

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let bytes = include_bytes!("../../app/icons/icon.png");
    let data = NSData::with_bytes(bytes);
    let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
        tracing::warn!("failed to decode bundled app icon PNG");
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    unsafe {
        app.setApplicationIconImage(Some(&image));
    }
    let dock_tile = app.dockTile();
    let mut tile_size = dock_tile.size();
    if tile_size.width <= 0.0 || tile_size.height <= 0.0 {
        tile_size = NSSize::new(128.0, 128.0);
    }
    let image_view = NSImageView::imageViewWithImage(&image, mtm);
    image_view.setFrame(NSRect::new(NSPoint::ZERO, tile_size));
    image_view.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
    dock_tile.setContentView(Some(&image_view));
    dock_tile.display();
}

#[cfg(not(target_os = "macos"))]
pub fn set_app_icon() {}

/// Applies the Dock icon now and re-applies shortly after so macOS Dock finishes binding the
/// tile after an `Accessory` -> `Regular` activation-policy transition or initial launch.
pub fn refresh_app_icon(cx: &mut App) {
    set_app_icon();
    if !cx.has_global::<AppGlobals>() {
        return;
    }
    let (tx1, rx1) = tokio::sync::oneshot::channel();
    let (tx2, rx2) = tokio::sync::oneshot::channel();
    cx.global::<AppGlobals>().backend.tokio.spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let _ = tx1.send(());
        tokio::time::sleep(std::time::Duration::from_millis(450)).await;
        let _ = tx2.send(());
    });
    cx.spawn(async move |cx| {
        if rx1.await.is_ok() {
            cx.update(|_| set_app_icon());
        }
        if rx2.await.is_ok() {
            cx.update(|_| set_app_icon());
        }
    })
    .detach();
}

fn activate_regular(cx: &mut App) {
    cx.set_activation_policy(ActivationPolicy::Regular);
    refresh_app_icon(cx);
    cx.activate(true);
}

/// Clears stale handles for `closed_id` and hides the Dock icon when no windows remain.
pub fn on_window_closed(closed_id: WindowId, cx: &mut App) {
    if cx.has_global::<AppGlobals>() {
        let globals = cx.global_mut::<AppGlobals>();
        if globals
            .chat
            .as_ref()
            .is_some_and(|(h, _)| h.window_id() == closed_id)
        {
            globals.chat = None;
        }
        if globals
            .settings
            .as_ref()
            .is_some_and(|(h, _)| h.window_id() == closed_id)
        {
            globals.settings = None;
        }
        if globals
            .setup
            .as_ref()
            .is_some_and(|(h, _)| h.window_id() == closed_id)
        {
            globals.setup = None;
        }
    }
    let has_other_windows = cx.windows().iter().any(|w| w.window_id() != closed_id);
    if has_other_windows {
        cx.set_activation_policy(ActivationPolicy::Regular);
        set_app_icon();
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
    activate_regular(cx);

    if let Some((handle, entity)) = cx.global::<AppGlobals>().chat.clone() {
        let ok = handle.update(cx, |_, window, cx| {
            if let Some(pid) = project_id.clone() {
                entity.update(cx, |view, cx| view.select_project(pid, window, cx));
            } else {
                entity.update(cx, |view, cx| view.focus_composer(window, cx));
            }
            window.activate_window();
        });
        if ok.is_ok() {
            return;
        }
    }

    let backend = cx.global::<AppGlobals>().backend.clone();
    let options = centered_options("Agent Superviser Chat", 1040.0, 720.0, cx);
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
    activate_regular(cx);

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
    let options = centered_options("Agent Superviser", 920.0, 680.0, cx);
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

pub fn open_project_settings(project_id: String, cx: &mut App) {
    activate_regular(cx);

    if let Some((handle, entity)) = cx.global::<AppGlobals>().settings.clone() {
        let ok = handle.update(cx, |_, window, cx| {
            entity.update(cx, |view, cx| {
                view.open_project_settings(&project_id, window, cx);
            });
            window.activate_window();
        });
        if ok.is_ok() {
            return;
        }
    }

    let backend = cx.global::<AppGlobals>().backend.clone();
    let options = centered_options("Agent Superviser", 920.0, 680.0, cx);
    match gpui_kit::open_window(options, cx, |window, cx| {
        apply_theme(Some(window), cx);
        cx.new(|cx| {
            let mut win = SettingsWindow::new(backend, 1, window, cx);
            win.open_project_settings(&project_id, window, cx);
            win
        })
    }) {
        Ok((handle, entity)) => {
            cx.global_mut::<AppGlobals>().settings = Some((handle, entity));
        }
        Err(e) => tracing::warn!("opening project settings window: {e:#}"),
    }
}

pub fn open_setup(cx: &mut App) {
    activate_regular(cx);

    if let Some((handle, _)) = cx.global::<AppGlobals>().setup.clone()
        && handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        return;
    }

    let backend = cx.global::<AppGlobals>().backend.clone();
    let options = centered_options("Set Up Agent Superviser", 800.0, 680.0, cx);
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

/// Prompts for a macOS folder natively via GPUI and invokes `on_pick` with the chosen path.
pub fn choose_folder(
    prompt_text: &'static str,
    on_pick: impl FnOnce(PathBuf, &mut App) + 'static,
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
        cx.update(|cx| on_pick(folder, cx));
    })
    .detach();
}

/// Prompts for a macOS folder natively via GPUI and creates/persists a real project for it.
pub fn choose_project_folder(prompt_text: &'static str, open_in_chat: bool, cx: &mut App) {
    choose_folder(
        prompt_text,
        move |folder, cx| {
            let app = cx.global::<AppGlobals>().backend.app.clone();
            match app.project_for_folder(&folder) {
                Ok(project) => {
                    cx.global::<AppGlobals>().refresh_tray();
                    if open_in_chat {
                        open_chat(Some(project.id), cx);
                    }
                }
                Err(e) => tracing::warn!("could not create project for {}: {e:#}", folder.display()),
            }
        },
        cx,
    );
}
