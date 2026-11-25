//! Agent Superviser — GPUI prototype.
//!
//! Validates the risky macOS menu-bar architecture (native `NSStatusItem` tray +
//! dynamic `ActivationPolicy::Accessory`/`Regular` Dock toggling + staying alive with
//! zero windows open + Tokio ↔ GPUI bridge) alongside live Chat, Settings, and Setup windows.
//!
//! Run with:
//!   cargo run -p agent-superviser-gpui
//!   cargo run -p agent-superviser-gpui -- --tray-only

mod backend;
mod chat;
mod settings;
mod setup;
mod theme;
mod tray;
mod windows;

use gpui_kit::{ActivationPolicy, QuitMode};

use crate::backend::{Backend, BackendEvent};
use crate::theme::apply_theme;
use crate::tray::{TrayAction, TrayController};
use crate::windows::{
    AppGlobals, choose_project_folder, open_chat, open_settings, open_setup, sync_dock_policy,
};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let tray_only = std::env::args().any(|a| a == "--tray-only");
    let backend = Backend::new().expect("initializing prototype backend");

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx| {
            gpui_kit::init(cx);
            apply_theme(None, cx);

            // Start as menu-bar accessory; opening a window flips to Regular.
            cx.set_activation_policy(ActivationPolicy::Accessory);

            let (tray, mut tray_rx) = match TrayController::new(&backend) {
                Ok((tray, rx)) => (Some(tray), Some(rx)),
                Err(e) => {
                    tracing::warn!("could not create menu bar tray icon: {e:#}");
                    (None, None)
                }
            };

            cx.set_global(AppGlobals {
                backend: backend.clone(),
                tray,
                chat: None,
                settings: None,
                setup: None,
            });

            // Closing the last window hides the Dock icon while keeping the menu bar app alive.
            cx.on_window_closed(|cx, _window_id| {
                sync_dock_policy(cx);
            })
            .detach();

            // Refresh the macOS tray menu whenever backend state changes (discovery, guard, models, projects).
            let mut backend_rx = backend.tx.subscribe();
            cx.spawn(async move |cx| {
                while let Ok(event) = backend_rx.recv().await {
                    if matches!(
                        event,
                        BackendEvent::DiscoveryUpdated
                            | BackendEvent::GuardCall
                            | BackendEvent::ProjectsChanged
                            | BackendEvent::ActiveModelChanged
                    ) {
                        cx.update(|cx| {
                            cx.global::<AppGlobals>().refresh_tray();
                        });
                    }
                }
            })
            .detach();

            // Handle macOS menu bar item clicks on GPUI's foreground executor (zero polling).
            if let Some(mut rx) = tray_rx.take() {
                cx.spawn(async move |cx| {
                    while let Some(action) = rx.recv().await {
                        cx.update(|cx| match action {
                            TrayAction::OpenChat(project) => open_chat(project, cx),
                            TrayAction::OpenSettings(tab) => open_settings(tab, cx),
                            TrayAction::OpenSetup => open_setup(cx),
                            TrayAction::ChooseProjectFolder => {
                                choose_project_folder("Choose a project folder", true, cx);
                            }
                            TrayAction::ToggleModel(model_id) => {
                                cx.global::<AppGlobals>()
                                    .backend
                                    .toggle_active_model(&model_id);
                                cx.global::<AppGlobals>().refresh_tray();
                            }
                            TrayAction::UnloadAll => {
                                *cx.global::<AppGlobals>()
                                    .backend
                                    .active_model
                                    .lock()
                                    .unwrap() = None;
                                let _ = cx
                                    .global::<AppGlobals>()
                                    .backend
                                    .tx
                                    .send(BackendEvent::ActiveModelChanged);
                                cx.global::<AppGlobals>().refresh_tray();
                            }
                            TrayAction::Quit => cx.quit(),
                        });
                    }
                })
                .detach();
            }

            if !tray_only {
                open_chat(None, cx);
            }
        });
}
