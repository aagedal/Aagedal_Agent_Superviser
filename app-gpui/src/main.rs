//! Agent Superviser — native GPUI menu bar app.
//!
//! Runs the shared `superviser-backend` runtime (llama-server, guard proxy, decision judge,
//! Hub downloads, OpenCode ACP, built-in agent, projects) with native macOS `NSStatusItem`
//! tray + dynamic Dock `ActivationPolicy::Accessory`/`Regular` toggling and Metal-rendered
//! Chat, Settings, and Setup windows.
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

use gpui_kit::{ActivationPolicy, KeyBinding, Menu, MenuItem, QuitMode};
use superviser_backend::AppEvent;
use superviser_guard::Verdict;

use crate::backend::Backend;
use crate::theme::apply_theme;
use crate::tray::{TrayAction, TrayController};
use crate::windows::{
    AppGlobals, choose_project_folder, on_window_closed, open_chat, open_settings, open_setup,
};

gpui_kit::actions!(
    superviser,
    [
        Quit,
        CloseWindow,
        MinimizeWindow,
        HideApp,
        OpenSettingsAction,
        OpenChatAction
    ]
);

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let tray_only = std::env::args().any(|a| a == "--tray-only");
    let backend = Backend::new().expect("initializing Agent Superviser backend");

    // Stop model servers cleanly if killed via SIGTERM / SIGINT (e.g. Activity Monitor / Ctrl+C).
    #[cfg(unix)]
    {
        let sig_backend = backend.clone();
        backend.tokio.spawn(async move {
            use tokio::signal::unix::{SignalKind, signal};
            let (Ok(mut term), Ok(mut interrupt)) = (
                signal(SignalKind::terminate()),
                signal(SignalKind::interrupt()),
            ) else {
                return;
            };
            tokio::select! {
                _ = term.recv() => {}
                _ = interrupt.recv() => {}
            }
            sig_backend.app.shutdown_async().await;
            std::process::exit(0);
        });
    }

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx| {
            gpui_kit::init(cx);
            apply_theme(None, cx);

            // Standard macOS keyboard shortcuts and native menu bar (Cmd+Q, Cmd+W, Cmd+,, Cmd+H, Cmd+M).
            cx.bind_keys([
                KeyBinding::new("cmd-q", Quit, None),
                KeyBinding::new("cmd-w", CloseWindow, None),
                KeyBinding::new("cmd-m", MinimizeWindow, None),
                KeyBinding::new("cmd-h", HideApp, None),
                KeyBinding::new("cmd-,", OpenSettingsAction, None),
                KeyBinding::new("cmd-n", OpenChatAction, None),
            ]);
            cx.on_action(|_: &Quit, cx| {
                cx.quit();
            });
            cx.on_action(|_: &CloseWindow, cx| {
                if let Some(handle) = cx.active_window() {
                    let _ = handle.update(cx, |_, window, _| window.remove_window());
                }
            });
            cx.on_action(|_: &MinimizeWindow, cx| {
                if let Some(handle) = cx.active_window() {
                    let _ = handle.update(cx, |_, window, _| window.minimize_window());
                }
            });
            cx.on_action(|_: &HideApp, cx| {
                cx.hide();
            });
            cx.on_action(|_: &OpenSettingsAction, cx| {
                open_settings(None, cx);
            });
            cx.on_action(|_: &OpenChatAction, cx| {
                open_chat(None, cx);
            });
            cx.set_menus(vec![
                Menu {
                    name: "Agent Superviser".into(),
                    disabled: false,
                    items: vec![
                        MenuItem::action("Settings…", OpenSettingsAction),
                        MenuItem::separator(),
                        MenuItem::action("Hide Agent Superviser", HideApp),
                        MenuItem::separator(),
                        MenuItem::action("Quit Agent Superviser", Quit),
                    ],
                },
                Menu {
                    name: "File".into(),
                    disabled: false,
                    items: vec![
                        MenuItem::action("Open Chat", OpenChatAction),
                        MenuItem::action("Close Window", CloseWindow),
                    ],
                },
                Menu {
                    name: "Window".into(),
                    disabled: false,
                    items: vec![MenuItem::action("Minimize", MinimizeWindow)],
                },
            ]);

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
            cx.on_window_closed(|cx, window_id| {
                on_window_closed(window_id, cx);
            })
            .detach();

            // Save conversations and stop model servers / VMs whenever the app quits (Cmd+Q or menu).
            let quit_backend = backend.clone();
            cx.on_app_quit(move |_cx| {
                let backend = quit_backend.clone();
                async move {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    std::thread::spawn(move || {
                        backend.shutdown();
                        let _ = tx.send(());
                    });
                    let _ = rx.await;
                }
            })
            .detach();

            // Refresh the macOS tray menu whenever backend state changes.
            let mut event_rx = backend.app.bus.subscribe();
            cx.spawn(async move |cx| {
                while let Some(event) = event_rx.recv().await {
                    let refresh = match &event {
                        AppEvent::Status(_)
                        | AppEvent::Discovery
                        | AppEvent::Projects
                        | AppEvent::Judge => true,
                        AppEvent::GuardCall(ev) => {
                            matches!(ev.verdict, Verdict::Blocked | Verdict::Flagged)
                        }
                        AppEvent::Download(dl) => dl.finished || dl.error.is_some(),
                        AppEvent::LoadProgress { .. }
                        | AppEvent::Reasoning
                        | AppEvent::Acp(_)
                        | AppEvent::Chat(_)
                        | AppEvent::Subagent(_) => false,
                    };
                    if refresh {
                        cx.update(|cx| {
                            cx.global::<AppGlobals>().refresh_tray();
                        });
                    }
                }
            })
            .detach();

            // Handle macOS menu bar item clicks on GPUI's foreground executor.
            if let Some(mut rx) = tray_rx.take() {
                cx.spawn(async move |cx| {
                    while let Some(action) = rx.recv().await {
                        cx.update(|cx| match action {
                            TrayAction::OpenChat(project) => open_chat(project, cx),
                            TrayAction::OpenProjectInOpenCode(project_id) => {
                                let app = cx.global::<AppGlobals>().backend.app.clone();
                                cx.global::<AppGlobals>().backend.tokio.spawn(async move {
                                    if let Err(e) = tokio::task::spawn_blocking(move || app.open_project(&project_id))
                                        .await
                                        .unwrap_or_else(|e| Err(e.into()))
                                    {
                                        tracing::warn!("open project in OpenCode failed: {e:#}");
                                    }
                                });
                            }
                            TrayAction::OpenSettings(tab) => open_settings(tab, cx),
                            TrayAction::OpenSetup => open_setup(cx),
                            TrayAction::ChooseProjectFolder => {
                                choose_project_folder("Choose a project folder", true, cx);
                            }
                            TrayAction::ToggleModel(model_id) => {
                                let backend = cx.global::<AppGlobals>().backend.clone();
                                if backend.app.loading_or_loaded(&model_id) {
                                    backend.spawn_unload_model(Some(model_id));
                                } else {
                                    let unload = match backend.app.room_for(&model_id) {
                                        Ok(room) => room.unload,
                                        Err(e) => {
                                            tracing::warn!("cannot load {model_id}: {e:#}");
                                            return;
                                        }
                                    };
                                    backend.spawn_load_model(model_id, unload);
                                }
                            }
                            TrayAction::UnloadAll => {
                                let backend = cx.global::<AppGlobals>().backend.clone();
                                backend.spawn_unload_model(None);
                            }
                            TrayAction::Quit => {
                                cx.quit();
                            }
                        });
                    }
                })
                .detach();
            }

            if !tray_only {
                if backend.app.needs_setup() {
                    open_setup(cx);
                } else {
                    open_chat(None, cx);
                }
            }
        });
}
