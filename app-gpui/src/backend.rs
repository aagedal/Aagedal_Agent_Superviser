//! GPUI host around the shared `superviser-backend` runtime and a Tokio executor.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use superviser_backend::App;
use superviser_core::catalog::{CatalogModel, Fit};
use superviser_core::hub::SearchHit;

/// Shared backend held by all GPUI windows and the macOS menu bar tray.
pub struct Backend {
    pub tokio: tokio::runtime::Runtime,
    pub app: Arc<App>,
    shutting_down: AtomicBool,
}

impl Backend {
    pub fn new() -> Result<Arc<Self>> {
        let tokio = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("superviser-tokio")
            .build()?;
        let app = Arc::new(App::new()?);

        // Start the real GuardServer on the configured port (falling back to any free loopback
        // port if another instance already binds it).
        let guard_app = app.clone();
        tokio.block_on(async move {
            if let Err(e) = guard_app.start_guard(Vec::new()).await {
                tracing::warn!("guard server could not start: {e:#}");
            }
        });

        // Scan other apps' model caches off the startup path.
        let scan_app = app.clone();
        std::thread::spawn(move || {
            scan_app.rescan_and_notify();
        });

        Ok(Arc::new(Self {
            tokio,
            app,
            shutting_down: AtomicBool::new(false),
        }))
    }

    /// Cleanly saves conversations and stops model servers, judge, OpenCode engines, VMs, and guard.
    /// Idempotent so signal handlers, `on_app_quit`, and menu Quit can all call it safely.
    pub fn shutdown(&self) {
        if self.shutting_down.swap(true, Ordering::SeqCst) {
            return;
        }
        self.app.shutdown(self.tokio.handle());
    }

    /// Runs a real Hugging Face GGUF search on the Tokio runtime.
    pub fn search_hf(
        self: &Arc<Self>,
        query: String,
    ) -> tokio::sync::oneshot::Receiver<std::result::Result<Vec<SearchHit>, String>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.tokio.spawn(async move {
            let res = App::hub()
                .search(&query, 20)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(res);
        });
        rx
    }

    /// Resolves a Hugging Face repo's default 4-bit quant and checks whether it fits on this machine.
    pub fn check_model(
        self: &Arc<Self>,
        repo: String,
    ) -> tokio::sync::oneshot::Receiver<std::result::Result<(CatalogModel, Option<Fit>), String>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let app = self.app.clone();
        self.tokio.spawn(async move {
            let res = CatalogModel::resolve(&App::hub(), &repo, None, "")
                .await
                .map(|model| {
                    let fit = app.fit(&model);
                    (model, fit)
                })
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(res);
        });
        rx
    }

    /// Loads `model_id`, optionally unloading `unload_first` to make room.
    pub fn load_model(
        self: &Arc<Self>,
        model_id: String,
        unload_first: Vec<String>,
    ) -> tokio::sync::oneshot::Receiver<std::result::Result<(), String>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let app = self.app.clone();
        self.tokio.spawn(async move {
            for old in unload_first {
                app.unload_model(Some(&old)).await;
            }
            let res = app.load_model(&model_id).await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(res);
        });
        rx
    }

    /// Fire-and-forget load helper (logs failures; UI observes state via `AppEvent::Status`).
    pub fn spawn_load_model(self: &Arc<Self>, model_id: String, unload_first: Vec<String>) {
        let app = self.app.clone();
        self.tokio.spawn(async move {
            for old in unload_first {
                app.unload_model(Some(&old)).await;
            }
            if let Err(e) = app.load_model(&model_id).await {
                tracing::warn!("loading {model_id}: {e:#}");
            }
        });
    }

    /// Unloads `model_id` (or all models if `None`).
    pub fn unload_model(
        self: &Arc<Self>,
        model_id: Option<String>,
    ) -> tokio::sync::oneshot::Receiver<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let app = self.app.clone();
        self.tokio.spawn(async move {
            app.unload_model(model_id.as_deref()).await;
            let _ = tx.send(());
        });
        rx
    }

    /// Fire-and-forget unload helper.
    pub fn spawn_unload_model(self: &Arc<Self>, model_id: Option<String>) {
        let app = self.app.clone();
        self.tokio.spawn(async move {
            app.unload_model(model_id.as_deref()).await;
        });
    }

    /// Starts downloading `model` on Tokio (entered into runtime context so `tokio::spawn` works).
    pub fn start_download_model(&self, model: CatalogModel) -> std::result::Result<String, String> {
        let _guard = self.tokio.enter();
        self.app
            .start_download_model(model)
            .map_err(|e| format!("{e:#}"))
    }

    /// Starts downloading the judge model on Tokio.
    pub fn start_download_judge(&self) -> std::result::Result<(), String> {
        let _guard = self.tokio.enter();
        self.app.start_download_judge().map_err(|e| format!("{e:#}"))
    }
}
