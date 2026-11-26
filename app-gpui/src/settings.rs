//! Settings window wired end-to-end to `superviser-backend`:
//! - Global active download progress bar with ETA and Cancel
//! - Models tab: Suggested picks, full catalog + HF search (Check fit + Download), On this Mac
//!   (Load / Unload / Room-check / Delete / Other apps rescan / Saved conversations)
//! - Projects tab: New project, Chat, Open in OpenCode, inline Project editor (isolation,
//!   related folders, internet, allowed hosts, tool groups, approvals, blocked programs, MCP off), Delete
//! - Safety tab: real d3-mini judge download & toggle (with fail-closed) + live guard call feed
//! - Connect tab: real OpenCode setup + auto-configure toggle, guard URL/API key + Regenerate, HF token

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::theme::{ActiveTheme, ThemeMode};
use gpui_kit::component::{Disableable, Sizable, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Anchor, App, AppContext, ClipboardItem, Context, Entity, InteractiveElement, IntoElement,
    ParentElement, Render, Styled, Subscription, Window, div, px,
};
use superviser_backend::files;
use superviser_backend::{AppEvent, DownloadEvent, JUDGE_DOWNLOAD, JudgeStatus, LoadState};
use superviser_core::catalog::{Catalog, CatalogModel, Fit, Role};
use superviser_core::hub::SearchHit;
use superviser_core::installed::{self, Source};
use superviser_core::project::{Approvals, Folder, Project, SUGGESTED_PROGRAMS, TOOL_GROUPS};
use superviser_core::sandbox::{self, Isolation, Network};
use superviser_core::{opencode, runtime, secrets};
use superviser_guard::Verdict;

use crate::backend::Backend;
use crate::setup::format_eta;
use crate::theme::{gib, palette, whole_gib};
use crate::windows::{
    choose_folder, choose_project_folder, open_chat, open_setup,
};

struct PendingLoad {
    model_id: String,
    model_name: String,
    unload_ids: Vec<String>,
    unload_names: Vec<String>,
}

struct ProjectEditor {
    project: Project,
    name_input: Entity<InputState>,
    hosts_input: Entity<InputState>,
    programs_input: Entity<InputState>,
}

pub struct SettingsWindow {
    backend: Arc<Backend>,
    active_tab: usize,
    model_filter: usize, // 0 = Suggested, 1 = On this Mac
    search_input: Entity<InputState>,
    hf_token_input: Entity<InputState>,
    searching: bool,
    search_results: Vec<SearchHit>,
    search_error: Option<String>,
    checking_hf: HashSet<String>,
    checked_hf: HashMap<String, Result<(CatalogModel, Option<Fit>), String>>,
    downloads: HashMap<String, (DownloadEvent, Instant)>,
    pending_load: Option<PendingLoad>,
    confirm_delete_model: Option<String>,
    confirm_delete_project: Option<String>,
    editing_project: Option<ProjectEditor>,
    notice: Option<(String, bool)>, // (message, is_error)
    rescanning: bool,
    reveal_key: bool,
    copied_field: Option<&'static str>,
    show_all_models: bool,
    _subscriptions: Vec<Subscription>,
}

impl SettingsWindow {
    pub fn new(
        backend: Arc<Backend>,
        initial_tab: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("e.g. unsloth coder"));
        let hf_token_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("hf_… (stored in macOS Keychain)"));

        let sub = cx.subscribe_in(
            &search_input,
            window,
            |this, _, event: &InputEvent, _window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.run_hf_search(cx);
                }
            },
        );

        let mut downloads = HashMap::new();
        for dl in backend.app.active_downloads() {
            downloads.insert(dl.id.clone(), (dl, Instant::now()));
        }

        // Live updates from shared backend EventBus.
        let mut rx = backend.app.bus.subscribe();
        cx.spawn(async move |this, cx| {
            while let Some(event) = rx.recv().await {
                let _ = this.update(cx, |this, cx| match event {
                    AppEvent::Download(dl) => {
                        if dl.finished || dl.error.is_some() {
                            this.downloads.remove(&dl.id);
                            if let Some(err) = dl.error {
                                this.notice = Some((format!("{}: {err}", dl.name), true));
                            } else {
                                this.notice = Some((format!("Downloaded {}.", dl.name), false));
                            }
                        } else {
                            let started = this
                                .downloads
                                .get(&dl.id)
                                .map(|(_, t)| *t)
                                .unwrap_or_else(Instant::now);
                            this.downloads.insert(dl.id.clone(), (dl, started));
                        }
                        cx.notify();
                    }
                    AppEvent::Status(_)
                    | AppEvent::LoadProgress { .. }
                    | AppEvent::GuardCall(_)
                    | AppEvent::Discovery
                    | AppEvent::Projects
                    | AppEvent::Judge
                    | AppEvent::Reasoning => {
                        cx.notify();
                    }
                    AppEvent::Acp(_) | AppEvent::Chat(_) | AppEvent::Subagent(_) => {}
                });
            }
        })
        .detach();

        Self {
            backend,
            active_tab: initial_tab.min(3),
            model_filter: 0,
            search_input,
            hf_token_input,
            searching: false,
            search_results: Vec::new(),
            search_error: None,
            checking_hf: HashSet::new(),
            checked_hf: HashMap::new(),
            downloads,
            pending_load: None,
            confirm_delete_model: None,
            confirm_delete_project: None,
            editing_project: None,
            notice: None,
            rescanning: false,
            reveal_key: false,
            copied_field: None,
            show_all_models: false,
            _subscriptions: vec![sub],
        }
    }

    pub fn set_tab(&mut self, tab: usize, cx: &mut Context<Self>) {
        self.active_tab = tab.min(3);
        cx.notify();
    }

    fn set_notice(&mut self, msg: impl Into<String>, is_error: bool, cx: &mut Context<Self>) {
        self.notice = Some((msg.into(), is_error));
        cx.notify();
    }

    fn run_hf_search(&mut self, cx: &mut Context<Self>) {
        let query = self.search_input.read(cx).value().trim().to_string();
        if query.is_empty() || self.searching {
            return;
        }
        self.searching = true;
        self.search_error = None;
        cx.notify();

        let rx = self.backend.search_hf(query);
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("search cancelled".into()));
            let _ = this.update(cx, |this, cx| {
                this.searching = false;
                match result {
                    Ok(hits) => this.search_results = hits,
                    Err(e) => {
                        this.search_results.clear();
                        this.search_error = Some(e);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn check_hf_repo(&mut self, repo: String, cx: &mut Context<Self>) {
        if !self.checking_hf.insert(repo.clone()) {
            return;
        }
        cx.notify();
        let rx = self.backend.check_model(repo.clone());
        cx.spawn(async move |this, cx| {
            let res = rx.await.unwrap_or_else(|_| Err("check cancelled".into()));
            let _ = this.update(cx, |this, cx| {
                this.checking_hf.remove(&repo);
                this.checked_hf.insert(repo, res);
                cx.notify();
            });
        })
        .detach();
    }

    fn start_download(&mut self, model: CatalogModel, cx: &mut Context<Self>) {
        match self.backend.start_download_model(model) {
            Ok(_) => {
                self.notice = None;
            }
            Err(e) => {
                self.notice = Some((e, true));
            }
        }
        cx.notify();
    }

    fn request_load_model(&mut self, model_id: String, model_name: String, cx: &mut Context<Self>) {
        match self.backend.app.room_for(&model_id) {
            Ok(room) if room.fits => {
                self.dispatch_load(model_id, Vec::new(), cx);
            }
            Ok(room) => {
                let loaded = self.backend.app.models();
                let unload_names: Vec<String> = room
                    .unload
                    .iter()
                    .map(|id| {
                        loaded
                            .iter()
                            .find(|m| &m.id == id)
                            .map(|m| m.name.clone())
                            .unwrap_or_else(|| id.clone())
                    })
                    .collect();
                self.pending_load = Some(PendingLoad {
                    model_id,
                    model_name,
                    unload_ids: room.unload,
                    unload_names,
                });
                cx.notify();
            }
            Err(e) => {
                self.set_notice(format!("{e:#}"), true, cx);
            }
        }
    }

    fn dispatch_load(
        &mut self,
        model_id: String,
        unload_first: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        self.pending_load = None;
        self.notice = None;
        let rx = self.backend.load_model(model_id, unload_first);
        cx.notify();
        cx.spawn(async move |this, cx| {
            if let Ok(Err(err)) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    this.notice = Some((err, true));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn dispatch_unload(&mut self, model_id: Option<String>, cx: &mut Context<Self>) {
        let rx = self.backend.unload_model(model_id);
        cx.spawn(async move |this, cx| {
            let _ = rx.await;
            let _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
    }

    fn trigger_rescan(&mut self, cx: &mut Context<Self>) {
        if self.rescanning {
            return;
        }
        self.rescanning = true;
        cx.notify();
        let app = self.backend.app.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.backend.tokio.spawn(async move {
            let _ = tokio::task::spawn_blocking(move || app.rescan_and_notify()).await;
            let _ = tx.send(());
        });
        cx.spawn(async move |this, cx| {
            let _ = rx.await;
            let _ = this.update(cx, |this, cx| {
                this.rescanning = false;
                cx.notify();
            });
        })
        .detach();
    }

    fn start_editing_project(
        &mut self,
        project: Project,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name_val = project.name.clone();
        let hosts_val = project.allowed_hosts.join(", ");
        let programs_val = project.blocked_programs.join(", ");

        let name_input = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("Project name");
            s.set_value(name_val, window, cx);
            s
        });
        let hosts_input = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("e.g. docs.rs, crates.io");
            s.set_value(hosts_val, window, cx);
            s
        });
        let programs_input = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("e.g. ssh, docker, sudo");
            s.set_value(programs_val, window, cx);
            s
        });

        self.editing_project = Some(ProjectEditor {
            project,
            name_input,
            hosts_input,
            programs_input,
        });
        self.confirm_delete_project = None;
        cx.notify();
    }

    fn save_editing_project(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.editing_project.take() else {
            return;
        };
        let mut project = editor.project;
        project.name = editor.name_input.read(cx).value().trim().to_string();
        project.allowed_hosts = split_tokens(&editor.hosts_input.read(cx).value());
        project.blocked_programs = split_tokens(&editor.programs_input.read(cx).value());

        match self.backend.app.save_project(project) {
            Ok(saved) => {
                self.set_notice(format!("Saved {}.", saved.name), false, cx);
            }
            Err(e) => {
                self.set_notice(format!("{e:#}"), true, cx);
            }
        }
    }

    fn copy_text(&mut self, field: &'static str, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.copied_field = Some(field);
        cx.notify();
    }

    fn info_popover(
        &self,
        id: impl Into<gpui_kit::ElementId>,
        model: &CatalogModel,
        fit: Option<&Fit>,
        cx: &App,
    ) -> impl IntoElement {
        let is_dark = cx.theme().mode == ThemeMode::Dark;
        let p = palette(is_dark);
        let title = model.name.clone();
        let desc = model.description.clone();
        let quant = format!("{}, {} download", model.quant.label, gib(model.quant.size));
        let context = fit
            .map(|f| format!("{}k tokens", f.context / 1024))
            .unwrap_or_else(|| "Too large for this machine".into());
        let mem = fit
            .map(|f| format!("about {} while running", gib(f.required)))
            .unwrap_or_else(|| "—".into());

        Popover::new(id)
            .anchor(Anchor::TopRight)
            .trigger(Button::new("info-btn").small().label("i"))
            .content(move |_, _, _| {
                v_flex()
                    .w(px(300.0))
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .child(title.clone()),
                    )
                    .when(!desc.is_empty(), |box_| {
                        box_.child(div().text_xs().text_color(p.muted).child(desc.clone()))
                    })
                    .child(
                        v_flex()
                            .gap_1()
                            .text_xs()
                            .child(format!("File: {quant}"))
                            .child(format!("Context: {context}"))
                            .child(format!("Memory: {mem}")),
                    )
            })
    }

    fn render_download_bar(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if self.downloads.is_empty() {
            return None;
        }
        let is_dark = cx.theme().mode == ThemeMode::Dark;
        let p = palette(is_dark);
        let mut active: Vec<(DownloadEvent, Instant)> = self.downloads.values().cloned().collect();
        active.sort_by(|a, b| a.0.name.cmp(&b.0.name));

        Some(
            v_flex()
                .flex_shrink_0()
                .px_5()
                .py_2()
                .gap_2()
                .bg(p.panel)
                .border_b_1()
                .border_color(p.line)
                .children(active.into_iter().enumerate().map(|(i, (dl, started))| {
                    let pct = if dl.total > 0 {
                        ((dl.done as f64 / dl.total as f64) * 100.0).clamp(0.0, 100.0)
                    } else {
                        0.0
                    };
                    let eta = format_eta(dl.done, dl.total, started);
                    let dl_id = dl.id.clone();
                    v_flex()
                        .gap_1()
                        .child(
                            h_flex()
                                .justify_between()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .text_xs()
                                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                                        .child(format!(
                                            "Downloading {} — {} / {} ({:.0}%){eta}",
                                            dl.name,
                                            gib(dl.done),
                                            gib(dl.total),
                                            pct
                                        )),
                                )
                                .child(
                                    Button::new(("cancel-dl", i))
                                        .small()
                                        .label("Cancel")
                                        .on_click(cx.listener(move |this, _, _, _| {
                                            this.backend.app.cancel_download(&dl_id);
                                        })),
                                ),
                        )
                        .child(
                            div()
                                .w_full()
                                .h(px(5.0))
                                .rounded_full()
                                .bg(p.chip)
                                .overflow_hidden()
                                .child(
                                    div()
                                        .h_full()
                                        .w(gpui_kit::relative((pct / 100.0) as f32))
                                        .bg(p.accent),
                                ),
                        )
                })),
        )
    }

    fn render_loading_bar(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let loading: Vec<(String, String, f32, String)> = self
            .backend
            .app
            .models()
            .into_iter()
            .filter_map(|m| match m.state {
                LoadState::Loading { progress, stage } => Some((
                    m.id,
                    m.name,
                    progress.unwrap_or(0.04).clamp(0.02, 0.99),
                    stage.unwrap_or_else(|| "Loading model…".into()),
                )),
                _ => None,
            })
            .collect();
        if loading.is_empty() {
            return None;
        }
        let is_dark = cx.theme().mode == ThemeMode::Dark;
        let p = palette(is_dark);
        Some(
            v_flex()
                .flex_shrink_0()
                .px_5()
                .py_2()
                .gap_2()
                .bg(p.panel)
                .border_b_1()
                .border_color(p.line)
                .children(loading.into_iter().enumerate().map(|(i, (mid, name, frac, stage))| {
                    v_flex()
                        .gap_1()
                        .child(
                            h_flex()
                                .justify_between()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .text_xs()
                                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                                        .child(format!(
                                            "Loading {name} — {stage} ({:.0}%)",
                                            frac * 100.0
                                        )),
                                )
                                .child(
                                    Button::new(("cancel-load", i))
                                        .small()
                                        .label("Cancel")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.dispatch_unload(Some(mid.clone()), cx);
                                        })),
                                ),
                        )
                        .child(
                            div()
                                .w_full()
                                .h(px(5.0))
                                .rounded_full()
                                .bg(p.chip)
                                .overflow_hidden()
                                .child(
                                    div()
                                        .h_full()
                                        .w(gpui_kit::relative(frac))
                                        .bg(p.accent),
                                ),
                        )
                })),
        )
    }

    fn render_model_action(
        &self,
        key_prefix: &'static str,
        idx: usize,
        catalog_model: &CatalogModel,
        installed: &[installed::InstalledModel],
        loaded: &[superviser_backend::LoadedModel],
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let catalog_id = installed::model_id(catalog_model);
        let local_match = installed
            .iter()
            .find(|m| m.id == catalog_id || m.model.repo.eq_ignore_ascii_case(&catalog_model.repo));
        let is_downloading = self.downloads.contains_key(&catalog_id);

        if let Some(local) = local_match {
            let loaded_entry = loaded.iter().find(|l| l.id == local.id);
            let lid = local.id.clone();
            let lname = local.model.name.clone();
            match loaded_entry.map(|l| &l.state) {
                Some(LoadState::Running { context }) => h_flex()
                    .gap_2()
                    .items_center()
                    .child(Tag::success().small().child(format!("Running ({}k)", context / 1024)))
                    .child(
                        Button::new((key_prefix, idx))
                            .small()
                            .label("Unload")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.dispatch_unload(Some(lid.clone()), cx);
                            })),
                    )
                    .into_any_element(),
                Some(LoadState::Loading { progress, .. }) => {
                    let label = match progress {
                        Some(f) => format!("Loading… {:.0}%", f * 100.0),
                        None => "Loading…".into(),
                    };
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(Tag::secondary().small().child(label))
                        .child(
                            Button::new((key_prefix, idx))
                                .small()
                                .label("Cancel")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.dispatch_unload(Some(lid.clone()), cx);
                                })),
                        )
                        .into_any_element()
                }
                _ => Button::new((key_prefix, idx))
                    .primary()
                    .small()
                    .label("Start")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.request_load_model(lid.clone(), lname.clone(), cx);
                    }))
                    .into_any_element(),
            }
        } else if is_downloading {
            Tag::secondary()
                .small()
                .child("Downloading…")
                .into_any_element()
        } else {
            let model_clone = catalog_model.clone();
            Button::new((key_prefix, idx))
                .small()
                .label(format!("Download ({})", whole_gib(catalog_model.quant.size)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.start_download(model_clone.clone(), cx);
                }))
                .into_any_element()
        }
    }

    fn render_models_tab(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_dark = cx.theme().mode == ThemeMode::Dark;
        let p = palette(is_dark);
        let hw = &self.backend.app.hardware;
        let model_hw = self.backend.app.model_hardware();
        let offers = self.backend.app.catalog.offers(&model_hw);
        let picks = Catalog::picks(&offers, hw.total_memory);
        let hidden = self
            .backend
            .app
            .catalog
            .models
            .len()
            .saturating_sub(offers.len());
        let installed = self.backend.app.installed();
        let loaded = self.backend.app.models();

        let local_label = if installed.is_empty() {
            "On this Mac".to_string()
        } else {
            format!("On this Mac ({})", installed.len())
        };

        v_flex()
            .gap_4()
            // Inline room confirmation when loading a model requires unloading another
            .when_some(self.pending_load.as_ref(), |col, pending| {
                let mid = pending.model_id.clone();
                let unload_ids = pending.unload_ids.clone();
                let names = pending.unload_names.join(" and ");
                let verb = if pending.unload_ids.len() == 1 {
                    "uses"
                } else {
                    "use"
                };
                col.child(
                    h_flex()
                        .p_3()
                        .gap_3()
                        .items_center()
                        .rounded(px(8.0))
                        .bg(p.warn.opacity(0.14))
                        .border_1()
                        .border_color(p.warn)
                        .child(
                            div()
                                .flex_1()
                                .text_xs()
                                .child(format!(
                                    "Loading {} needs the memory {names} {verb}. Unload {names} to make room?",
                                    pending.model_name
                                )),
                        )
                        .child(
                            Button::new("confirm-room-load")
                                .primary()
                                .small()
                                .label("Unload & load")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.dispatch_load(mid.clone(), unload_ids.clone(), cx);
                                })),
                        )
                        .child(
                            Button::new("cancel-room-load")
                                .small()
                                .label("Cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.pending_load = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(
                h_flex().child(
                    TabBar::new("model-filter")
                        .segmented()
                        .selected_index(self.model_filter)
                        .on_click(cx.listener(|this, ix, _, cx| {
                            this.model_filter = *ix;
                            cx.notify();
                        }))
                        .child(Tab::new().label("Suggested"))
                        .child(Tab::new().label(local_label)),
                ),
            )
            .when(self.model_filter == 0, |col| {
                let kind = if hw.unified_memory {
                    "This Mac"
                } else {
                    "This computer"
                };
                col.child(
                    v_flex()
                        .gap_1()
                        .child(
                            div()
                                .text_lg()
                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                .child("Pick a model"),
                        )
                        .child(div().text_sm().text_color(p.muted).child(format!(
                            "{kind} has {} of memory. Up to {} can go to a model. Only models that fit are shown.",
                            whole_gib(hw.total_memory),
                            gib(model_hw.model_budget)
                        ))),
                )
                .child(
                    v_flex()
                        .gap_2()
                        .children(picks.iter().enumerate().map(|(i, pick)| {
                            let role = match pick.role {
                                Role::MostCapable => "Most capable",
                                Role::Fast => "Capable and fast",
                                Role::Light => "Lighter",
                                Role::Basic => "For basic tasks",
                            };
                            let left = hw.total_memory.saturating_sub(pick.offer.fit.required);
                            let summary = format!(
                                "{} · Leaves {} for your other apps",
                                if pick.offer.fast {
                                    "Quick replies"
                                } else {
                                    "Slower replies"
                                },
                                whole_gib(left)
                            );
                            let suggested = i == 0;

                            h_flex()
                                .p_3()
                                .gap_3()
                                .items_center()
                                .rounded(px(10.0))
                                .bg(p.panel)
                                .border_1()
                                .border_color(if suggested { p.accent } else { p.line })
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .gap_1()
                                        .child(
                                            h_flex()
                                                .gap_2()
                                                .items_baseline()
                                                .child(
                                                    div()
                                                        .text_sm()
                                                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                                        .child(role),
                                                )
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(p.muted)
                                                        .child(pick.offer.model.name.clone()),
                                                ),
                                        )
                                        .child(div().text_xs().text_color(p.muted).child(summary))
                                        .when(suggested, |body| {
                                            body.child(
                                                h_flex().mt_1().child(
                                                    Tag::primary()
                                                        .small()
                                                        .child("Good place to start"),
                                                ),
                                            )
                                        }),
                                )
                                .child(self.info_popover(
                                    ("pick-info", i),
                                    &pick.offer.model,
                                    Some(&pick.offer.fit),
                                    cx,
                                ))
                                .child(self.render_model_action(
                                    "pick-act",
                                    i,
                                    &pick.offer.model,
                                    &installed,
                                    &loaded,
                                    cx,
                                ))
                        })),
                )
                // Expandable full catalog + Hugging Face search
                .child(
                    v_flex()
                        .gap_2()
                        .pt_2()
                        .child(
                            Button::new("toggle-all-models")
                                .link()
                                .small()
                                .label(if self.show_all_models {
                                    format!(
                                        "▾ Hide all {} catalog models & Hugging Face search",
                                        offers.len()
                                    )
                                } else {
                                    format!(
                                        "▸ All {} catalog models & Hugging Face search ({hidden} too large)",
                                        offers.len()
                                    )
                                })
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.show_all_models = !this.show_all_models;
                                    cx.notify();
                                })),
                        )
                        .when(self.show_all_models, |more| {
                            more.child(
                                v_flex()
                                    .gap_2()
                                    .children(offers.iter().enumerate().map(|(i, offer)| {
                                        h_flex()
                                            .p_3()
                                            .gap_3()
                                            .items_center()
                                            .rounded(px(8.0))
                                            .bg(p.panel)
                                            .border_1()
                                            .border_color(p.line)
                                            .child(
                                                v_flex()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .child(
                                                        div()
                                                            .text_sm()
                                                            .font_weight(
                                                                gpui_kit::FontWeight::SEMIBOLD,
                                                            )
                                                            .child(offer.model.name.clone()),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_xs()
                                                            .text_color(p.muted)
                                                            .child(offer.model.description.clone()),
                                                    ),
                                            )
                                            .child(self.info_popover(
                                                ("offer-info", i),
                                                &offer.model,
                                                Some(&offer.fit),
                                                cx,
                                            ))
                                            .child(self.render_model_action(
                                                "offer-act",
                                                i,
                                                &offer.model,
                                                &installed,
                                                &loaded,
                                                cx,
                                            ))
                                    })),
                            )
                            .child(
                                v_flex()
                                    .mt_3()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                            .child("Search Hugging Face (GGUF models)"),
                                    )
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .child(div().flex_1().child(Input::new(&self.search_input)))
                                            .child(
                                                Button::new("hf-search")
                                                    .primary()
                                                    .label(if self.searching {
                                                        "Searching…"
                                                    } else {
                                                        "Search"
                                                    })
                                                    .disabled(self.searching)
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.run_hf_search(cx);
                                                    })),
                                            ),
                                    )
                                    .when_some(self.search_error.clone(), |box_, err| {
                                        box_.child(
                                            div()
                                                .p_2()
                                                .rounded(px(6.0))
                                                .bg(p.bad.opacity(0.14))
                                                .text_color(p.bad)
                                                .text_xs()
                                                .child(err),
                                        )
                                    })
                                    .children(self.search_results.iter().enumerate().map(|(i, hit)| {
                                        let repo = hit.id.clone();
                                        let checking = self.checking_hf.contains(&repo);
                                        let checked = self.checked_hf.get(&repo).cloned();
                                        v_flex()
                                            .id(("hf-hit", i))
                                            .p_2()
                                            .gap_2()
                                            .rounded(px(8.0))
                                            .bg(p.panel)
                                            .border_1()
                                            .border_color(p.line)
                                            .child(
                                                h_flex()
                                                    .justify_between()
                                                    .items_center()
                                                    .gap_2()
                                                    .child(
                                                        v_flex()
                                                            .flex_1()
                                                            .min_w_0()
                                                            .child(div().text_sm().truncate().child(hit.id.clone()))
                                                            .child(
                                                                h_flex()
                                                                    .gap_1()
                                                                    .child(Tag::secondary().small().child(format!(
                                                                        "{} downloads",
                                                                        hit.downloads
                                                                    )))
                                                                    .child(Tag::secondary().small().child(format!(
                                                                        "{} likes",
                                                                        hit.likes
                                                                    ))),
                                                            ),
                                                    )
                                                    .child(match &checked {
                                                        Some(Ok((model, Some(_)))) => {
                                                            self.render_model_action(
                                                                "hf-dl",
                                                                i,
                                                                model,
                                                                &installed,
                                                                &loaded,
                                                                cx,
                                                            )
                                                        }
                                                        Some(Ok((_, None))) => Tag::secondary()
                                                            .small()
                                                            .child("Too large")
                                                            .into_any_element(),
                                                        _ => {
                                                            let repo_btn = repo.clone();
                                                            Button::new(("hf-check", i))
                                                                .small()
                                                                .disabled(checking)
                                                                .label(if checking {
                                                                    "Checking…"
                                                                } else {
                                                                    "Check fit"
                                                                })
                                                                .on_click(cx.listener(
                                                                    move |this, _, _, cx| {
                                                                        this.check_hf_repo(
                                                                            repo_btn.clone(),
                                                                            cx,
                                                                        );
                                                                    },
                                                                ))
                                                                .into_any_element()
                                                        }
                                                    }),
                                            )
                                            .when_some(checked, |row, res| match res {
                                                Ok((model, Some(fit))) => row.child(
                                                    div().text_xs().text_color(p.muted).child(
                                                        format!(
                                                            "{} · {} download · {}k context (~{} in memory)",
                                                            model.quant.label,
                                                            gib(model.quant.size),
                                                            fit.context / 1024,
                                                            gib(fit.required)
                                                        ),
                                                    ),
                                                ),
                                                Ok((model, None)) => row.child(
                                                    div().text_xs().text_color(p.warn).child(
                                                        format!(
                                                            "{} ({}) is too large for this machine's memory budget.",
                                                            model.quant.label,
                                                            gib(model.quant.size)
                                                        ),
                                                    ),
                                                ),
                                                Err(err) => row.child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(p.bad)
                                                        .child(err),
                                                ),
                                            })
                                    })),
                            )
                        }),
                )
            })
            .when(self.model_filter == 1, |col| {
                let discovery = self.backend.app.discovery.lock().unwrap();
                let found_other = discovery.models.len();
                let mlx_count = discovery.mlx.len();
                drop(discovery);
                let (use_other, save_convs) = {
                    let s = self.backend.app.settings.lock().unwrap();
                    (s.use_other_apps, s.save_conversations)
                };
                let conv_summary = self.backend.app.conversations.summary();

                col.child(
                    div()
                        .text_lg()
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .child("On this Mac"),
                )
                .when(installed.is_empty(), |box_| {
                    box_.child(
                        div()
                            .text_sm()
                            .text_color(p.muted)
                            .child("No local GGUF models found yet. Pick one from Suggested to download."),
                    )
                })
                .children(installed.iter().enumerate().map(|(i, m)| {
                    let mid = m.id.clone();
                    let mname = m.model.name.clone();
                    let loaded_entry = loaded.iter().find(|l| l.id == m.id);
                    let is_loaded = loaded_entry.is_some_and(|l| !matches!(l.state, LoadState::Failed { .. }));
                    let fits = self.backend.app.fit(&m.model).is_some();
                    let removable = m.source == Source::App;
                    let confirming_del = self.confirm_delete_model.as_deref() == Some(m.id.as_str());

                    let state_badge = match loaded_entry.map(|l| &l.state) {
                        Some(LoadState::Running { context }) => {
                            Some((format!("Running ({}k)", context / 1024), p.good))
                        }
                        Some(LoadState::Loading { progress, stage }) => {
                            let text = match (stage, progress) {
                                (Some(st), Some(f)) => format!("{st} ({:.0}%)", f * 100.0),
                                (Some(st), None) => st.clone(),
                                (None, Some(f)) => format!("Loading… {:.0}%", f * 100.0),
                                (None, None) => "Loading…".into(),
                            };
                            Some((text, p.accent))
                        }
                        Some(LoadState::Failed { error }) => Some((format!("Failed: {error}"), p.bad)),
                        None => None,
                    };

                    v_flex()
                        .p_3()
                        .gap_2()
                        .rounded(px(10.0))
                        .bg(p.panel)
                        .border_1()
                        .border_color(p.line)
                        .child(
                            h_flex()
                                .gap_3()
                                .items_center()
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .gap_1()
                                        .child(
                                            div()
                                                .text_sm()
                                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                                .child(format!(
                                                    "{}{}",
                                                    m.model.name,
                                                    if is_loaded { " ●" } else { "" }
                                                )),
                                        )
                                        .child(
                                            h_flex()
                                                .flex_wrap()
                                                .gap_1()
                                                .child(Tag::secondary().small().child(format!(
                                                    "{} · {}",
                                                    m.model.quant.label,
                                                    gib(m.model.quant.size)
                                                )))
                                                .when(m.source != Source::App, |tags| {
                                                    tags.child(
                                                        Tag::success()
                                                            .small()
                                                            .child(format!("from {}", m.source.label())),
                                                    )
                                                }),
                                        )
                                        .when_some(state_badge, |col, (text, color)| {
                                            col.child(div().text_xs().text_color(color).child(text))
                                        }),
                                )
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(if is_loaded {
                                            let mid_u = mid.clone();
                                            Button::new(("local-unload", i))
                                                .small()
                                                .label("Unload")
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.dispatch_unload(Some(mid_u.clone()), cx);
                                                }))
                                        } else {
                                            let mid_l = mid.clone();
                                            let mname_l = mname.clone();
                                            Button::new(("local-load", i))
                                                .primary()
                                                .small()
                                                .disabled(!fits)
                                                .label(if fits { "Load" } else { "Too large" })
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.request_load_model(
                                                        mid_l.clone(),
                                                        mname_l.clone(),
                                                        cx,
                                                    );
                                                }))
                                        })
                                        .when(removable && !confirming_del, |row| {
                                            let mid_d = mid.clone();
                                            row.child(
                                                Button::new(("local-del", i))
                                                    .ghost()
                                                    .small()
                                                    .disabled(is_loaded)
                                                    .label("Delete")
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        this.confirm_delete_model = Some(mid_d.clone());
                                                        cx.notify();
                                                    })),
                                            )
                                        }),
                                ),
                        )
                        .when(confirming_del, |card| {
                            let mid_d = mid.clone();
                            card.child(
                                h_flex()
                                    .p_2()
                                    .gap_2()
                                    .items_center()
                                    .rounded(px(6.0))
                                    .bg(p.bad.opacity(0.12))
                                    .child(
                                        div()
                                            .flex_1()
                                            .text_xs()
                                            .child(format!("Delete {mname} from disk?")),
                                    )
                                    .child(
                                        Button::new(("confirm-del-model", i))
                                            .danger()
                                            .small()
                                            .label("Delete")
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.confirm_delete_model = None;
                                                if let Err(e) = this.backend.app.delete_model(&mid_d) {
                                                    this.set_notice(format!("{e:#}"), true, cx);
                                                } else {
                                                    cx.notify();
                                                }
                                            })),
                                    )
                                    .child(
                                        Button::new(("cancel-del-model", i))
                                            .small()
                                            .label("Cancel")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.confirm_delete_model = None;
                                                cx.notify();
                                            })),
                                    ),
                            )
                        })
                }))
                // Other apps discovery settings
                .child(
                    v_flex()
                        .p_3()
                        .gap_2()
                        .rounded(px(10.0))
                        .bg(p.panel)
                        .border_1()
                        .border_color(p.line)
                        .child(
                            h_flex()
                                .justify_between()
                                .items_center()
                                .child(
                                    Checkbox::new("use-other-apps")
                                        .checked(use_other)
                                        .label("Use models other apps already downloaded (LM Studio, Ollama, HF cache)")
                                        .on_click(cx.listener(|this, checked, _, cx| {
                                            this.backend.app.settings.lock().unwrap().use_other_apps = *checked;
                                            if let Err(e) = this.backend.app.save_settings() {
                                                this.set_notice(format!("{e:#}"), true, cx);
                                                return;
                                            }
                                            this.trigger_rescan(cx);
                                        })),
                                )
                                .child(
                                    Button::new("rescan-models")
                                        .small()
                                        .disabled(self.rescanning)
                                        .label(if self.rescanning { "Scanning…" } else { "Rescan" })
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.trigger_rescan(cx);
                                        })),
                                ),
                        )
                        .child(div().text_xs().text_color(p.muted).child(format!(
                            "Found {found_other} usable GGUF models from other apps and {mlx_count} MLX models."
                        ))),
                )
                // Saved conversations card
                .child(
                    v_flex()
                        .p_3()
                        .gap_2()
                        .rounded(px(10.0))
                        .bg(p.panel)
                        .border_1()
                        .border_color(p.line)
                        .child(
                            h_flex()
                                .justify_between()
                                .items_center()
                                .child(
                                    Checkbox::new("save-conversations")
                                        .checked(save_convs)
                                        .label("Save each model's last conversation to disk for instant resume")
                                        .on_click(cx.listener(|this, checked, _, cx| {
                                            if let Err(e) = this.backend.app.set_save_conversations(*checked) {
                                                this.set_notice(format!("{e:#}"), true, cx);
                                            } else {
                                                cx.notify();
                                            }
                                        })),
                                )
                                .when(conv_summary.count > 0, |row| {
                                    row.child(
                                        Button::new("clear-convs")
                                            .small()
                                            .label("Clear saved")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.backend.app.conversations.clear();
                                                cx.notify();
                                            })),
                                    )
                                }),
                        )
                        .child(div().text_xs().text_color(p.muted).child(if conv_summary.count > 0 {
                            format!(
                                "{} saved ({} on disk, limit {}).",
                                conv_summary.count,
                                gib(conv_summary.bytes),
                                gib(conv_summary.limit)
                            )
                        } else {
                            "No saved model conversations on disk yet.".into()
                        })),
                )
            })
    }

    fn render_project_editor(
        &self,
        editor: &ProjectEditor,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_dark = cx.theme().mode == ThemeMode::Dark;
        let p = palette(is_dark);
        let proj = &editor.project;
        let choices = sandbox::choices(proj.main());
        let mcp_servers: Vec<String> = opencode::mcp_servers()
            .into_iter()
            .map(|s| s.name)
            .collect();

        v_flex()
            .mt_2()
            .p_3()
            .gap_3()
            .rounded(px(8.0))
            .bg(p.bg)
            .border_1()
            .border_color(p.accent)
            // Name
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_xs().text_color(p.muted).child("Project name"))
                    .child(Input::new(&editor.name_input)),
            )
            // Isolation
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_xs().text_color(p.muted).child("Isolation"))
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap_2()
                            .children(choices.into_iter().enumerate().map(|(i, choice)| {
                                let selected = proj.isolation == choice.isolation;
                                let iso = choice.isolation;
                                let unavailable = choice.unavailable.clone();
                                let setup_note = choice.setup.clone();
                                v_flex().gap_1().child(
                                    h_flex().gap_1().items_center().child(
                                        Button::new(("iso-choice", i))
                                            .small()
                                            .when(selected, |b| b.primary())
                                            .disabled(unavailable.is_some())
                                            .label(match iso {
                                                Isolation::MacosSandbox => "macOS sandbox",
                                                Isolation::LinuxVm => "Linux VM",
                                            })
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                if let Some(ed) = &mut this.editing_project {
                                                    ed.project.isolation = iso;
                                                    cx.notify();
                                                }
                                            })),
                                    )
                                    .when(setup_note.is_some(), |row| {
                                        row.child(
                                            Button::new(("iso-install", i))
                                                .link()
                                                .small()
                                                .label("Get installer…")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    let (tx, rx) = tokio::sync::oneshot::channel();
                                                    this.backend.tokio.spawn(async move {
                                                        let url = sandbox::container::installer_url().await;
                                                        let res = files::open_url(&url)
                                                            .map(|_| url)
                                                            .map_err(|e| format!("{e}"));
                                                        let _ = tx.send(res);
                                                    });
                                                    cx.spawn(async move |this, cx| {
                                                        if let Ok(Err(e)) = rx.await {
                                                            let _ = this.update(cx, |this, cx| {
                                                                this.set_notice(e, true, cx);
                                                            });
                                                        }
                                                    })
                                                    .detach();
                                                })),
                                        )
                                    }),
                                )
                                .when_some(unavailable, |col, why| {
                                    col.child(div().text_xs().text_color(p.muted).child(why))
                                })
                            })),
                    ),
            )
            // Related folders
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .child(div().text_xs().text_color(p.muted).child("Related folders"))
                            .child(
                                Button::new("add-related-folder")
                                    .small()
                                    .label("+ Add folder…")
                                    .on_click(cx.listener(|_this, _, _, cx| {
                                        let view = cx.entity().downgrade();
                                        choose_folder(
                                            "Choose a related folder",
                                            move |folder, cx| {
                                                let _ = view.update(cx, |this, cx| {
                                                    if let Some(ed) = &mut this.editing_project
                                                        && !ed
                                                            .project
                                                            .folders
                                                            .iter()
                                                            .any(|f| f.path == folder)
                                                    {
                                                        ed.project.folders.push(Folder {
                                                            path: folder,
                                                            writable: false,
                                                        });
                                                        cx.notify();
                                                    }
                                                });
                                            },
                                            cx,
                                        );
                                    })),
                            ),
                    )
                    .children(proj.related().iter().enumerate().map(|(i, folder)| {
                        let folder_idx = i + 1;
                        let writable = folder.writable;
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .text_xs()
                                    .truncate()
                                    .child(folder.path.display().to_string()),
                            )
                            .child(
                                Button::new(("rel-rw", i))
                                    .small()
                                    .label(if writable { "Writable" } else { "Read-only" })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if let Some(ed) = &mut this.editing_project
                                            && let Some(f) = ed.project.folders.get_mut(folder_idx)
                                        {
                                            f.writable = !f.writable;
                                            cx.notify();
                                        }
                                    })),
                            )
                            .child(
                                Button::new(("rel-rm", i))
                                    .ghost()
                                    .small()
                                    .label("×")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if let Some(ed) = &mut this.editing_project
                                            && folder_idx < ed.project.folders.len()
                                        {
                                            ed.project.folders.remove(folder_idx);
                                            cx.notify();
                                        }
                                    })),
                            )
                    })),
            )
            // Network & Allowed hosts
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        Checkbox::new("proj-net")
                            .checked(proj.network == Network::Internet)
                            .label("Allow internet access inside the sandbox")
                            .on_click(cx.listener(|this, checked, _, cx| {
                                if let Some(ed) = &mut this.editing_project {
                                    ed.project.network = if *checked {
                                        Network::Internet
                                    } else {
                                        Network::LocalOnly
                                    };
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(p.muted)
                                    .child("Allowed websites (guard won't block these even if unmentioned)"),
                            )
                            .child(Input::new(&editor.hosts_input)),
                    ),
            )
            // Agent tools
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_xs().text_color(p.muted).child("Agent tools"))
                    .children(TOOL_GROUPS.iter().enumerate().map(|(i, group)| {
                        let key = group.key;
                        let allowed = proj.allows(key);
                        Checkbox::new(("tool-group", i))
                            .checked(allowed)
                            .label(group.label)
                            .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                if let Some(ed) = &mut this.editing_project {
                                    ed.project.tools_off.retain(|k| k != key);
                                    if !*checked {
                                        ed.project.tools_off.push(key.to_string());
                                    }
                                    cx.notify();
                                }
                            }))
                    })),
            )
            // Approvals
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(p.muted)
                            .child("Ask before acting (built-in agent)"),
                    )
                    .child(h_flex().gap_2().children(
                        [
                            (Approvals::Commands, "Before commands"),
                            (Approvals::Everything, "Commands & file edits"),
                            (Approvals::Never, "Never ask"),
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(i, (val, label))| {
                            Button::new(("appr", i))
                                .small()
                                .when(proj.approvals == val, |b| b.primary())
                                .label(label)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(ed) = &mut this.editing_project {
                                        ed.project.approvals = val;
                                        cx.notify();
                                    }
                                }))
                        }),
                    )),
            )
            // Blocked programs
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(p.muted)
                            .child("Blocked programs (comma-separated)"),
                    )
                    .child(Input::new(&editor.programs_input))
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap_1()
                            .children(SUGGESTED_PROGRAMS.iter().enumerate().map(|(i, prog)| {
                                let prog_str = *prog;
                                Button::new(("sug-prog", i))
                                    .ghost()
                                    .small()
                                    .label(format!("+{prog_str}"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        if let Some(ed) = &mut this.editing_project {
                                            let cur = ed.programs_input.read(cx).value().to_string();
                                            let mut list = split_tokens(&cur);
                                            if !list.iter().any(|x| x == prog_str) {
                                                list.push(prog_str.to_string());
                                                let joined = list.join(", ");
                                                ed.programs_input.update(cx, |s, cx| {
                                                    s.set_value(joined, window, cx);
                                                });
                                            }
                                        }
                                    }))
                            })),
                    ),
            )
            // MCP servers off
            .when(!mcp_servers.is_empty(), |col| {
                col.child(
                    v_flex()
                        .gap_1()
                        .child(
                            div()
                                .text_xs()
                                .text_color(p.muted)
                                .child("OpenCode MCP servers (uncheck to disable for this project)"),
                        )
                        .children(mcp_servers.into_iter().enumerate().map(|(i, srv)| {
                            let enabled = !proj.mcp_off.iter().any(|m| m == &srv);
                            let srv_name = srv.clone();
                            Checkbox::new(("mcp-srv", i))
                                .checked(enabled)
                                .label(srv)
                                .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                    if let Some(ed) = &mut this.editing_project {
                                        ed.project.mcp_off.retain(|m| m != &srv_name);
                                        if !*checked {
                                            ed.project.mcp_off.push(srv_name.clone());
                                        }
                                        cx.notify();
                                    }
                                }))
                        })),
                )
            })
            // Save / Cancel
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("cancel-edit-proj")
                            .small()
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.editing_project = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("save-edit-proj")
                            .primary()
                            .small()
                            .label("Save changes")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.save_editing_project(cx);
                            })),
                    ),
            )
    }

    fn render_projects_tab(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_dark = cx.theme().mode == ThemeMode::Dark;
        let p = palette(is_dark);
        let projects = self.backend.app.settings.lock().unwrap().projects.clone();
        let has_running_model = self.backend.app.latest_model().is_some();

        v_flex()
            .gap_3()
            .child(
                div()
                    .text_lg()
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .child("Projects"),
            )
            .child(div().text_sm().text_color(p.muted).child(
                "A project is a main folder, optional related folders, and the isolation, network, and tool rules enforced by both the sandbox and the guard.",
            ))
            .child(
                h_flex().gap_2().child(
                    Button::new("new-project")
                        .primary()
                        .label("New project…")
                        .on_click(|_, _, cx| {
                            choose_project_folder("Choose your project's main folder", false, cx);
                        }),
                ),
            )
            .when(projects.is_empty(), |box_| {
                box_.child(
                    div()
                        .text_sm()
                        .text_color(p.muted)
                        .child("No projects yet. Pick a folder to add one."),
                )
            })
            .children(projects.iter().enumerate().map(|(i, proj)| {
                let pid = proj.id.clone();
                let pid_chat = pid.clone();
                let pid_open = pid.clone();
                let pid_del = pid.clone();
                let proj_clone = proj.clone();
                let is_editing = self
                    .editing_project
                    .as_ref()
                    .is_some_and(|ed| ed.project.id == proj.id);
                let confirming_del = self.confirm_delete_project.as_deref() == Some(proj.id.as_str());
                let iso_label = match proj.isolation {
                    Isolation::MacosSandbox => "macOS sandbox",
                    Isolation::LinuxVm => "Linux VM",
                };
                let net_label = if proj.network == Network::Internet {
                    "Internet on"
                } else {
                    "Internet off"
                };

                v_flex()
                    .p_3()
                    .gap_2()
                    .rounded(px(10.0))
                    .bg(p.panel)
                    .border_1()
                    .border_color(p.line)
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                            .child(proj.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(p.muted)
                                            .truncate()
                                            .child(proj.main().display().to_string()),
                                    )
                                    .child(
                                        h_flex()
                                            .flex_wrap()
                                            .gap_1()
                                            .child(Tag::secondary().small().child(iso_label))
                                            .child(Tag::secondary().small().child(net_label))
                                            .when(!proj.related().is_empty(), |tags| {
                                                tags.child(Tag::secondary().small().child(format!(
                                                    "+{} folder{}",
                                                    proj.related().len(),
                                                    if proj.related().len() == 1 { "" } else { "s" }
                                                )))
                                            }),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        Button::new(("proj-chat", i))
                                            .primary()
                                            .small()
                                            .label("Chat")
                                            .on_click(move |_, _, cx| {
                                                open_chat(Some(pid_chat.clone()), cx);
                                            }),
                                    )
                                    .child(
                                        Button::new(("proj-term", i))
                                            .small()
                                            .disabled(!has_running_model)
                                            .label("OpenCode")
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                let app = this.backend.app.clone();
                                                let id = pid_open.clone();
                                                let (tx, rx) = tokio::sync::oneshot::channel();
                                                this.backend.tokio.spawn(async move {
                                                    let res = tokio::task::spawn_blocking(move || {
                                                        app.open_project(&id)
                                                    })
                                                    .await
                                                    .unwrap_or_else(|e| Err(e.into()))
                                                    .map_err(|e| format!("{e:#}"));
                                                    let _ = tx.send(res);
                                                });
                                                cx.spawn(async move |this, cx| {
                                                    if let Ok(Err(e)) = rx.await {
                                                        let _ = this.update(cx, |this, cx| {
                                                            this.set_notice(e, true, cx);
                                                        });
                                                    }
                                                })
                                                .detach();
                                            })),
                                    )
                                    .child(
                                        Button::new(("proj-edit", i))
                                            .small()
                                            .label(if is_editing { "Close" } else { "Edit" })
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                if is_editing {
                                                    this.editing_project = None;
                                                    cx.notify();
                                                } else {
                                                    this.start_editing_project(
                                                        proj_clone.clone(),
                                                        window,
                                                        cx,
                                                    );
                                                }
                                            })),
                                    )
                                    .child(
                                        Button::new(("proj-del", i))
                                            .ghost()
                                            .small()
                                            .label("Delete")
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.confirm_delete_project = Some(pid_del.clone());
                                                cx.notify();
                                            })),
                                    ),
                            ),
                    )
                    .when(confirming_del, |card| {
                        let pid_confirm = pid.clone();
                        let pname = proj.name.clone();
                        card.child(
                            h_flex()
                                .p_2()
                                .gap_2()
                                .items_center()
                                .rounded(px(6.0))
                                .bg(p.bad.opacity(0.12))
                                .child(div().flex_1().text_xs().child(format!(
                                    "Remove project \"{pname}\" from Agent Superviser? (Your files on disk are not touched.)"
                                )))
                                .child(
                                    Button::new(("confirm-del-proj", i))
                                        .danger()
                                        .small()
                                        .label("Remove")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.confirm_delete_project = None;
                                            if this
                                                .editing_project
                                                .as_ref()
                                                .is_some_and(|e| e.project.id == pid_confirm)
                                            {
                                                this.editing_project = None;
                                            }
                                            if let Err(e) = this.backend.app.delete_project(&pid_confirm) {
                                                this.set_notice(format!("{e:#}"), true, cx);
                                            } else {
                                                cx.notify();
                                            }
                                        })),
                                )
                                .child(
                                    Button::new(("cancel-del-proj", i))
                                        .small()
                                        .label("Cancel")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.confirm_delete_project = None;
                                            cx.notify();
                                        })),
                                ),
                        )
                    })
                    .when(is_editing, |card| {
                        if let Some(editor) = &self.editing_project {
                            card.child(self.render_project_editor(editor, cx))
                        } else {
                            card
                        }
                    })
            }))
    }

    fn render_safety_tab(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_dark = cx.theme().mode == ThemeMode::Dark;
        let p = palette(is_dark);
        let events: Vec<_> = self
            .backend
            .app
            .events
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect();
        let (judge_on, judge_fail_closed) = {
            let s = self.backend.app.settings.lock().unwrap();
            (s.judge, s.judge_fail_closed)
        };
        let judge_model = self.backend.app.catalog.judge.clone();
        let judge_installed = self.backend.app.judge_installed().is_some();
        let judge_fits = self.backend.app.judge_fits();
        let judge_downloading = self.downloads.contains_key(JUDGE_DOWNLOAD);
        let judge_status = self.backend.app.judge_status.lock().unwrap().clone();
        let judge_mem = judge_model
            .as_ref()
            .map_or(0, |j| j.judge_required());
        let (judge_status_label, judge_status_color) = match &judge_status {
            JudgeStatus::Idle => (
                if judge_on {
                    "Idle (starts when a chat model loads)".to_string()
                } else {
                    "Off".to_string()
                },
                p.muted,
            ),
            JudgeStatus::Loading => ("Loading d3-mini…".into(), p.accent),
            JudgeStatus::Running => ("Running".into(), p.good),
            JudgeStatus::Failed { error } => (format!("Failed: {error}"), p.bad),
        };

        v_flex()
            .gap_3()
            .child(
                div()
                    .text_lg()
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .child("Safety"),
            )
            .child(div().text_sm().text_color(p.muted).child(
                "The guard checks every tool call the model makes before the agent runs it. High-risk calls are blocked immediately; suspicious (WARN) calls can optionally be reviewed by the local d3-mini decision judge.",
            ))
            .child(
                v_flex()
                    .p_3()
                    .gap_2()
                    .rounded(px(10.0))
                    .bg(p.panel)
                    .border_1()
                    .border_color(p.line)
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .child("Decision judge (d3-mini)"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(judge_status_color)
                                    .child(judge_status_label),
                            ),
                    )
                    .child(div().text_xs().text_color(p.muted).child(if judge_fits {
                        format!(
                            "Uses about {} of memory beside your chat model when running.",
                            gib(judge_mem)
                        )
                    } else {
                        "This Mac does not have enough memory to run the judge beside a chat model."
                            .into()
                    }))
                    .when(judge_fits && !judge_installed, |card| {
                        let dl_size = judge_model
                            .as_ref()
                            .map_or(0, |m| m.quant.size);
                        card.child(
                            h_flex().gap_2().child(
                                Button::new("dl-judge")
                                    .primary()
                                    .small()
                                    .disabled(judge_downloading)
                                    .label(if judge_downloading {
                                        "Downloading safety check model…"
                                    } else {
                                        "Download safety check model"
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Err(e) = this.backend.start_download_judge() {
                                            this.set_notice(e, true, cx);
                                        } else {
                                            cx.notify();
                                        }
                                    })),
                            )
                            .child(div().text_xs().text_color(p.muted).child(gib(dl_size))),
                        )
                    })
                    .child(
                        Checkbox::new("judge-enabled")
                            .checked(judge_on)
                            .disabled(!judge_fits || !judge_installed)
                            .label("Check WARN calls with the decision judge")
                            .on_click(cx.listener(move |this, checked, _, cx| {
                                let enable = *checked;
                                let (tx, rx) = tokio::sync::oneshot::channel();
                                let app = this.backend.app.clone();
                                this.backend.tokio.spawn(async move {
                                    let res = app
                                        .set_judge(enable, judge_fail_closed)
                                        .await
                                        .map_err(|e| format!("{e:#}"));
                                    let _ = tx.send(res);
                                });
                                cx.spawn(async move |this, cx| {
                                    if let Ok(Err(e)) = rx.await {
                                        let _ = this.update(cx, |this, cx| {
                                            this.set_notice(e, true, cx);
                                        });
                                    }
                                })
                                .detach();
                            })),
                    )
                    .child(
                        Checkbox::new("judge-fail-closed")
                            .checked(judge_fail_closed)
                            .disabled(!judge_on)
                            .label("Block the call if the judge doesn't answer within 30 seconds")
                            .on_click(cx.listener(move |this, checked, _, cx| {
                                let fc = *checked;
                                let (tx, rx) = tokio::sync::oneshot::channel();
                                let app = this.backend.app.clone();
                                this.backend.tokio.spawn(async move {
                                    let res = app
                                        .set_judge(judge_on, fc)
                                        .await
                                        .map_err(|e| format!("{e:#}"));
                                    let _ = tx.send(res);
                                });
                                cx.spawn(async move |this, cx| {
                                    if let Ok(Err(e)) = rx.await {
                                        let _ = this.update(cx, |this, cx| {
                                            this.set_notice(e, true, cx);
                                        });
                                    }
                                })
                                .detach();
                            })),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .child(format!("Recent tool calls ({})", events.len())),
            )
            .when(events.is_empty(), |box_| {
                box_.child(
                    div()
                        .text_sm()
                        .text_color(p.muted)
                        .child("No tool calls recorded yet. Calls made by OpenCode or the chat agent appear here live."),
                )
            })
            .children(events.iter().enumerate().map(|(i, ev)| {
                let (label, color) = match ev.verdict {
                    Verdict::Passed => ("PASS", p.good),
                    Verdict::Suspicious => ("WARN", p.warn),
                    Verdict::Flagged => ("FLAG", p.warn),
                    Verdict::Blocked => ("BLOCK", p.bad),
                };
                let judge_note = ev.judge.as_ref().map(|j| format!("Judge: {j}"));
                h_flex()
                    .id(("call-row", i))
                    .py_2()
                    .gap_3()
                    .border_b_1()
                    .border_color(p.line)
                    .child(
                        div()
                            .w(px(52.0))
                            .text_xs()
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .text_color(color)
                            .child(label),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_xs()
                                    .child(format!("{}: {}", ev.tool, ev.summary)),
                            )
                            .when_some(ev.reason.clone(), |col, reason| {
                                col.child(div().text_xs().text_color(p.muted).child(reason))
                            })
                            .when_some(judge_note, |col, note| {
                                col.child(div().text_xs().text_color(p.muted).child(note))
                            }),
                    )
            }))
    }

    fn render_connect_tab(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_dark = cx.theme().mode == ThemeMode::Dark;
        let p = palette(is_dark);
        let base_url = self
            .backend
            .app
            .guard_url()
            .unwrap_or_else(|| "(guard not running)".into());
        let api_key = secrets::local_api_key(&self.backend.app.paths)
            .unwrap_or_else(|_| "(not generated yet)".into());
        let display_key = if self.reveal_key {
            api_key.clone()
        } else {
            "•".repeat(api_key.len().clamp(12, 32))
        };
        let opencode_ver = opencode::version()
            .map(|v| format!("OpenCode {v} found."))
            .unwrap_or_else(|| "OpenCode not found (built-in agent will be used for chat).".into());
        let llama_ver = runtime::find_llama_server()
            .map(|path| format!("llama-server found at {}", path.display()))
            .unwrap_or_else(|| "llama-server not found on PATH.".into());
        let configure_opencode = self
            .backend
            .app
            .settings
            .lock()
            .unwrap()
            .configure_opencode;
        let has_running = self.backend.app.latest_model().is_some();
        let hf_token_set = secrets::hf_token().is_some();

        let base_url_copy = base_url.clone();
        let api_key_copy = api_key.clone();

        v_flex()
            .gap_3()
            .child(
                div()
                    .text_lg()
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .child("Connect a coding tool"),
            )
            // OpenCode card
            .child(
                v_flex()
                    .p_3()
                    .gap_2()
                    .rounded(px(10.0))
                    .bg(p.panel)
                    .border_1()
                    .border_color(p.line)
                    .child(div().text_sm().child(opencode_ver))
                    .child(div().text_xs().text_color(p.muted).child(llama_ver))
                    .child(
                        Checkbox::new("auto-opencode")
                            .checked(configure_opencode)
                            .label("Update OpenCode's global config automatically whenever a model starts")
                            .on_click(cx.listener(|this, checked, _, cx| {
                                this.backend
                                    .app
                                    .settings
                                    .lock()
                                    .unwrap()
                                    .configure_opencode = *checked;
                                if let Err(e) = this.backend.app.save_settings() {
                                    this.set_notice(format!("{e:#}"), true, cx);
                                } else {
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        h_flex().gap_2().items_center().child(
                            Button::new("setup-opencode-now")
                                .primary()
                                .small()
                                .disabled(!has_running)
                                .label("Set up OpenCode now")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    match this.backend.app.setup_opencode() {
                                        Ok(res) => {
                                            let msg = format!(
                                                "Updated {}{}",
                                                res.path.display(),
                                                if res.restarted_service {
                                                    " and restarted OpenCode."
                                                } else {
                                                    "."
                                                }
                                            );
                                            this.set_notice(msg, false, cx);
                                        }
                                        Err(e) => {
                                            this.set_notice(format!("{e:#}"), true, cx);
                                        }
                                    }
                                })),
                        )
                        .when(!has_running, |row| {
                            row.child(
                                div()
                                    .text_xs()
                                    .text_color(p.muted)
                                    .child("Load a model first to configure OpenCode."),
                            )
                        }),
                    ),
            )
            // OpenAI-compatible endpoint card
            .child(
                v_flex()
                    .p_3()
                    .gap_2()
                    .rounded(px(10.0))
                    .bg(p.panel)
                    .border_1()
                    .border_color(p.line)
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .child("OpenAI-compatible endpoint"),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().w(px(72.0)).text_xs().text_color(p.muted).child("Base URL"))
                            .child(
                                div()
                                    .flex_1()
                                    .px_2()
                                    .py_1()
                                    .rounded(px(6.0))
                                    .bg(p.bg)
                                    .border_1()
                                    .border_color(p.line)
                                    .text_xs()
                                    .child(base_url),
                            )
                            .child(
                                Button::new("copy-url")
                                    .small()
                                    .label(if self.copied_field == Some("url") {
                                        "Copied"
                                    } else {
                                        "Copy"
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.copy_text("url", base_url_copy.clone(), cx);
                                    })),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().w(px(72.0)).text_xs().text_color(p.muted).child("API key"))
                            .child(
                                div()
                                    .flex_1()
                                    .px_2()
                                    .py_1()
                                    .rounded(px(6.0))
                                    .bg(p.bg)
                                    .border_1()
                                    .border_color(p.line)
                                    .text_xs()
                                    .child(display_key),
                            )
                            .child(
                                Button::new("toggle-key")
                                    .small()
                                    .label(if self.reveal_key { "Hide" } else { "Show" })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.reveal_key = !this.reveal_key;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("copy-key")
                                    .small()
                                    .label(if self.copied_field == Some("key") {
                                        "Copied"
                                    } else {
                                        "Copy"
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.copy_text("key", api_key_copy.clone(), cx);
                                    })),
                            )
                            .child(
                                Button::new("regen-key")
                                    .small()
                                    .label("Regenerate")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        match this.backend.app.regenerate_api_key() {
                                            Ok(_) => this.set_notice(
                                                "Generated a new local API key. Re-run OpenCode setup if needed.",
                                                false,
                                                cx,
                                            ),
                                            Err(e) => this.set_notice(format!("{e:#}"), true, cx),
                                        }
                                    })),
                            ),
                    ),
            )
            // Hugging Face token card
            .child(
                v_flex()
                    .p_3()
                    .gap_2()
                    .rounded(px(10.0))
                    .bg(p.panel)
                    .border_1()
                    .border_color(p.line)
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .child("Hugging Face token (optional, for gated models)"),
                            )
                            .child(
                                Tag::secondary()
                                    .small()
                                    .child(if hf_token_set { "Saved in Keychain" } else { "Not set" }),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().child(Input::new(&self.hf_token_input)))
                            .child(
                                Button::new("save-hf-token")
                                    .primary()
                                    .small()
                                    .label("Save")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        let val = this.hf_token_input.read(cx).value().trim().to_string();
                                        if val.is_empty() {
                                            return;
                                        }
                                        match secrets::set_hf_token(Some(&val)) {
                                            Ok(()) => {
                                                this.hf_token_input.update(cx, |s, cx| {
                                                    s.set_value("", window, cx);
                                                });
                                                this.set_notice("Saved Hugging Face token to Keychain.", false, cx);
                                            }
                                            Err(e) => this.set_notice(format!("{e:#}"), true, cx),
                                        }
                                    })),
                            )
                            .when(hf_token_set, |row| {
                                row.child(
                                    Button::new("clear-hf-token")
                                        .small()
                                        .label("Clear")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            match secrets::set_hf_token(None) {
                                                Ok(()) => this.set_notice("Removed Hugging Face token.", false, cx),
                                                Err(e) => this.set_notice(format!("{e:#}"), true, cx),
                                            }
                                        })),
                                )
                            }),
                    ),
            )
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_dark = cx.theme().mode == ThemeMode::Dark;
        let p = palette(is_dark);

        v_flex()
            .size_full()
            .bg(p.bg)
            .text_color(p.text)
            .child(
                h_flex()
                    .flex_shrink_0()
                    .px_4()
                    .pt_2()
                    .items_center()
                    .justify_between()
                    .bg(p.panel)
                    .border_b_1()
                    .border_color(p.line)
                    .child(
                        TabBar::new("settings-tabs")
                            .underline()
                            .selected_index(self.active_tab)
                            .on_click(cx.listener(|this, ix, _, cx| {
                                this.active_tab = *ix;
                                cx.notify();
                            }))
                            .child(Tab::new().label("Models"))
                            .child(Tab::new().label("Projects"))
                            .child(Tab::new().label("Safety"))
                            .child(Tab::new().label("Connect")),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .pb_1()
                            .child(
                                Button::new("nav-chat")
                                    .small()
                                    .label("Open Chat")
                                    .on_click(|_, _, cx| open_chat(None, cx)),
                            )
                            .child(
                                Button::new("nav-setup")
                                    .ghost()
                                    .small()
                                    .label("Setup guide")
                                    .on_click(|_, _, cx| open_setup(cx)),
                            ),
                    ),
            )
            .when_some(self.render_download_bar(cx), |col, bar| col.child(bar))
            .when_some(self.render_loading_bar(cx), |col, bar| col.child(bar))
            .when_some(self.notice.clone(), |col, (msg, is_error)| {
                let color = if is_error { p.bad } else { p.good };
                col.child(
                    h_flex()
                        .flex_shrink_0()
                        .px_5()
                        .py_2()
                        .gap_2()
                        .items_center()
                        .justify_between()
                        .bg(color.opacity(0.14))
                        .border_b_1()
                        .border_color(p.line)
                        .text_color(color)
                        .text_xs()
                        .child(div().flex_1().child(msg))
                        .child(
                            Button::new("dismiss-notice")
                                .ghost()
                                .small()
                                .label("Dismiss")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.notice = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(
                        div()
                            .id("settings-scroll")
                            .size_full()
                            .overflow_y_scrollbar()
                            .child(v_flex().w_full().p_5().child(match self.active_tab {
                                0 => self.render_models_tab(cx).into_any_element(),
                                1 => self.render_projects_tab(cx).into_any_element(),
                                2 => self.render_safety_tab(cx).into_any_element(),
                                _ => self.render_connect_tab(cx).into_any_element(),
                            })),
                    ),
            )
    }
}

fn split_tokens(input: &str) -> Vec<String> {
    input
        .split([',', '\n', ' '])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}
