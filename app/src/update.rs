//! Updates from the project's GitHub releases. Each is notarized by Apple and, on top of that,
//! signed with the project's updater key, which the updater checks (with the version it was
//! signed for) before installing. Only release builds that carry the key look for updates.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use superviser_core::awake::Awake;
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_updater::{Update, UpdaterExt};

/// First look a little after launch, then twice a day.
const FIRST_CHECK: Duration = Duration::from_secs(30);
const CHECK_EVERY: Duration = Duration::from_secs(12 * 60 * 60);

/// The update ready to install, offered in the menu bar menu, and whether it's being installed.
#[derive(Default)]
pub struct Updates {
    ready: Mutex<Option<Update>>,
    installing: AtomicBool,
}

impl Updates {
    /// The new version, and whether it's being installed.
    pub fn available(&self) -> Option<(String, bool)> {
        self.ready.lock().unwrap().as_ref().map(|u| (u.version.clone(), self.installing.load(Ordering::Relaxed)))
    }
}

/// Whether this build can update itself: a release build with the updater's public key.
fn enabled(handle: &AppHandle) -> bool {
    let pubkey = handle.config().plugins.0.get("updater").and_then(|u| u["pubkey"].as_str()).unwrap_or_default();
    !cfg!(debug_assertions) && !pubkey.is_empty()
}

/// Looks for updates in the background for as long as the app runs.
pub fn start(handle: &AppHandle) {
    if !enabled(handle) {
        return;
    }
    let handle = handle.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(FIRST_CHECK).await;
        loop {
            match check(&handle).await {
                Ok(Some(update)) => {
                    tracing::info!("version {} is available", update.version);
                    *handle.state::<Updates>().ready.lock().unwrap() = Some(update);
                    crate::refresh_tray(&handle);
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("checking for updates: {e:#}"),
            }
            tokio::time::sleep(CHECK_EVERY).await;
        }
    });
}

async fn check(handle: &AppHandle) -> anyhow::Result<Option<Update>> {
    Ok(handle.updater()?.check().await?)
}

/// Installs the update and restarts, after asking if that would stop work under way.
pub fn install(handle: &AppHandle) {
    let updates = handle.state::<Updates>();
    let Some(update) = updates.ready.lock().unwrap().clone() else { return };
    if updates.installing.load(Ordering::Relaxed) {
        return;
    }
    let version = update.version.clone();
    let owned = handle.clone();
    let go = move |confirmed: bool| {
        let handle = owned;
        if !confirmed {
            return;
        }
        handle.state::<Updates>().installing.store(true, Ordering::Relaxed);
        crate::refresh_tray(&handle);
        tauri::async_runtime::spawn(async move {
            tracing::info!("installing version {}", update.version);
            match update.download_and_install(|_, _| {}, || {}).await {
                Ok(()) => handle.request_restart(),
                Err(e) => {
                    handle.state::<Updates>().installing.store(false, Ordering::Relaxed);
                    crate::refresh_tray(&handle);
                    handle.dialog().message(format!("{e:#}")).title("Couldn't install the update").kind(MessageDialogKind::Error).show(|_| {});
                }
            }
        });
    };
    if Awake::held() {
        handle
            .dialog()
            .message("An agent is working or a model is downloading. Restarting stops it; a download picks up where it left off.")
            .title(format!("Update to {version} and restart now?"))
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::OkCancelCustom("Update and Restart".into(), "Later".into()))
            .show(go);
    } else {
        go(true);
    }
}
