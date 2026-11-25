//! Settings window GPUI prototype:
//! 4 tabs matching `app/ui/settings.html` (Models, Projects, Safety, Connect)
//! wired to real `superviser-core` hardware/catalog/discovery/HF search/projects
//! and a live `superviser-guard` call stream.

use std::sync::Arc;

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
use superviser_core::catalog::{CatalogModel, Fit, Role};
use superviser_core::hub::SearchHit;
use superviser_core::installed::Source;
use superviser_core::sandbox::Network;
use superviser_guard::Verdict;

use crate::backend::Backend;
use crate::theme::{gib, palette, whole_gib};
use crate::windows::{AppGlobals, choose_project_folder, open_chat, open_setup};

pub struct SettingsWindow {
    backend: Arc<Backend>,
    active_tab: usize,
    model_filter: usize, // 0 = Suggested, 1 = On this Mac
    search_input: Entity<InputState>,
    searching: bool,
    search_results: Vec<SearchHit>,
    search_error: Option<String>,
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
        let search_input = cx.new(|cx| InputState::new(window, cx).placeholder("e.g. unsloth coder"));

        let sub = cx.subscribe_in(
            &search_input,
            window,
            |this, _, event: &InputEvent, _window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.run_hf_search(cx);
                }
            },
        );

        // Live updates from Tokio backend (discovery finished, guard call arrived, projects changed).
        let mut rx = backend.tx.subscribe();
        cx.spawn(async move |this, cx| {
            while let Ok(_event) = rx.recv().await {
                let _ = this.update(cx, |_, cx| cx.notify());
            }
        })
        .detach();

        Self {
            backend,
            active_tab: initial_tab.min(3),
            model_filter: 0,
            search_input,
            searching: false,
            search_results: Vec::new(),
            search_error: None,
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

    fn render_models_tab(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_dark = cx.theme().mode == ThemeMode::Dark;
        let p = palette(is_dark);
        let hw = &self.backend.hardware;
        let (picks, offers, hidden) = self.backend.catalog_offers();
        let installed = self.backend.installed();
        let active_id = self.backend.active_model_id();

        let local_label = if installed.is_empty() {
            "On this Mac".to_string()
        } else {
            format!("On this Mac ({})", installed.len())
        };

        v_flex()
            .gap_4()
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
                            gib(self.backend.model_hardware().model_budget)
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
                                if pick.offer.fast { "Quick replies" } else { "Slower replies" },
                                whole_gib(left)
                            );
                            let suggested = i == 0;
                            let model_id = format!("{}:{}", pick.offer.model.repo, pick.offer.model.quant.label);
                            let is_installed = installed.iter().any(|m| m.id == model_id);
                            let is_active = active_id.as_deref() == Some(model_id.as_str());

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
                                .child(self.info_popover(("pick-info", i), &pick.offer.model, Some(&pick.offer.fit), cx))
                                .child(if is_active {
                                    Tag::success().small().child("Active").into_any_element()
                                } else if is_installed {
                                    Button::new(("pick-load", i))
                                        .primary()
                                        .small()
                                        .label("Load")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.backend.toggle_active_model(&model_id);
                                            cx.global::<AppGlobals>().refresh_tray();
                                            cx.notify();
                                        }))
                                        .into_any_element()
                                } else {
                                    Button::new(("pick-dl", i))
                                        .small()
                                        .label(format!("Download ({})", whole_gib(pick.offer.model.quant.size)))
                                        .disabled(true)
                                        .into_any_element()
                                })
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
                                    format!("▾ Hide all {} catalog models & Hugging Face search", offers.len())
                                } else {
                                    format!("▸ All {} catalog models & Hugging Face search ({hidden} too large)", offers.len())
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
                                                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
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
                                            .child("Search Hugging Face (live Tokio query)"),
                                    )
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .child(div().flex_1().child(Input::new(&self.search_input)))
                                            .child(
                                                Button::new("hf-search")
                                                    .primary()
                                                    .label(if self.searching { "Searching…" } else { "Search" })
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
                                        h_flex()
                                            .id(("hf-hit", i))
                                            .p_2()
                                            .rounded(px(8.0))
                                            .bg(p.panel)
                                            .border_1()
                                            .border_color(p.line)
                                            .justify_between()
                                            .items_center()
                                            .child(div().text_sm().child(hit.id.clone()))
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
                                            )
                                    })),
                            )
                        }),
                )
            })
            .when(self.model_filter == 1, |col| {
                let discovery = self.backend.discovery.lock().unwrap();
                let found_other = discovery.models.len();
                let mlx_count = discovery.mlx.len();
                drop(discovery);

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
                            .child("No local GGUF models found yet."),
                    )
                })
                .children(installed.iter().enumerate().map(|(i, m)| {
                    let mid = m.id.clone();
                    let active = active_id.as_deref() == Some(m.id.as_str());
                    let fits = self.backend.fit(&m.model).is_some();
                    h_flex()
                        .p_3()
                        .gap_3()
                        .items_center()
                        .rounded(px(10.0))
                        .bg(p.panel)
                        .border_1()
                        .border_color(p.line)
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
                                            if active { " ●" } else { "" }
                                        )),
                                )
                                .child(
                                    h_flex()
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
                                ),
                        )
                        .child(
                            Button::new(("local-toggle", i))
                                .small()
                                .when(!active, |b| b.primary())
                                .disabled(!fits && !active)
                                .label(if active { "Unload" } else { "Load (prototype)" })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.backend.toggle_active_model(&mid);
                                    cx.global::<AppGlobals>().refresh_tray();
                                    cx.notify();
                                })),
                        )
                }))
                .child(
                    div()
                        .mt_2()
                        .text_xs()
                        .text_color(p.muted)
                        .child(format!(
                            "Background scan found {found_other} usable models from other apps (LM Studio, Ollama, HF cache) and {mlx_count} MLX models."
                        )),
                )
            })
    }

    fn render_projects_tab(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_dark = cx.theme().mode == ThemeMode::Dark;
        let p = palette(is_dark);
        let projects = self.backend.projects();

        v_flex()
            .gap_3()
            .child(
                div()
                    .text_lg()
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .child("Projects"),
            )
            .child(div().text_sm().text_color(p.muted).child(
                "A project is what you code on: a main folder, related folders, and what the agent may do there. Clicking \"New project…\" below tests GPUI's native async macOS folder picker (`cx.prompt_for_paths`).",
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
                let net_label = if proj.network == Network::Internet {
                    "Internet on"
                } else {
                    "Internet off"
                };
                h_flex()
                    .p_3()
                    .gap_3()
                    .items_center()
                    .rounded(px(10.0))
                    .bg(p.panel)
                    .border_1()
                    .border_color(p.line)
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
                                    .gap_1()
                                    .child(Tag::secondary().small().child(format!("{:?}", proj.isolation)))
                                    .child(Tag::secondary().small().child(net_label)),
                            ),
                    )
                    .child(
                        Button::new(("proj-chat", i))
                            .primary()
                            .small()
                            .label("Chat")
                            .on_click(move |_, _, cx| open_chat(Some(pid.clone()), cx)),
                    )
            }))
    }

    fn render_safety_tab(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_dark = cx.theme().mode == ThemeMode::Dark;
        let p = palette(is_dark);
        let events = self.backend.recent_events();
        let (judge_on, judge_fail_closed) = {
            let s = self.backend.settings.lock().unwrap();
            (s.judge, s.judge_fail_closed)
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
                "The guard checks every command and file edit the model asks for. Use the buttons below to fire a real `CallEvent` from Tokio and verify live updates in both this list and the macOS menu bar counter.",
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
                        Checkbox::new("judge-enabled")
                            .checked(judge_on)
                            .label("Check WARN calls with the decision judge (prototype toggle)")
                            .on_click(cx.listener(|this, checked, _, cx| {
                                this.backend.settings.lock().unwrap().judge = *checked;
                                cx.notify();
                            })),
                    )
                    .child(
                        Checkbox::new("judge-fail-closed")
                            .checked(judge_fail_closed)
                            .disabled(!judge_on)
                            .label("Block the call if the judge doesn't answer within 30 seconds")
                            .on_click(cx.listener(|this, checked, _, cx| {
                                this.backend.settings.lock().unwrap().judge_fail_closed = *checked;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("sim-block")
                            .danger()
                            .small()
                            .label("Simulate blocked call")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.backend.simulate_guard_event(true);
                                cx.global::<AppGlobals>().refresh_tray();
                            })),
                    )
                    .child(
                        Button::new("sim-pass")
                            .small()
                            .label("Simulate passed call")
                            .on_click(cx.listener(|this, _, _, _cx| {
                                this.backend.simulate_guard_event(false);
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
                        .child("No tool calls yet. Click \"Simulate blocked call\" above to test."),
                )
            })
            .children(events.iter().enumerate().map(|(i, ev)| {
                let (label, color) = match ev.verdict {
                    Verdict::Passed => ("PASS", p.good),
                    Verdict::Suspicious => ("WARN", p.warn),
                    Verdict::Flagged => ("FLAG", p.warn),
                    Verdict::Blocked => ("BLOCK", p.bad),
                };
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
                            }),
                    )
            }))
    }

    fn render_connect_tab(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_dark = cx.theme().mode == ThemeMode::Dark;
        let p = palette(is_dark);
        let base_url = self
            .backend
            .guard_url()
            .unwrap_or_else(|| "(guard not running)".into());
        let api_key = self.backend.local_api_key();
        let display_key = if self.reveal_key {
            api_key.clone()
        } else {
            "•".repeat(api_key.len().clamp(12, 32))
        };
        let opencode_ver = self
            .backend
            .opencode_version()
            .map(|v| format!("OpenCode {v} found."))
            .unwrap_or_else(|| "OpenCode not found on PATH.".into());
        let llama_ver = self
            .backend
            .llama_server()
            .map(|p| format!("llama-server found at {}", p.display()))
            .unwrap_or_else(|| "llama-server not found on PATH.".into());

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
            .child(
                v_flex()
                    .p_3()
                    .gap_1()
                    .rounded(px(10.0))
                    .bg(p.panel)
                    .border_1()
                    .border_color(p.line)
                    .child(div().text_sm().child(opencode_ver))
                    .child(div().text_xs().text_color(p.muted).child(llama_ver)),
            )
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
                            .child("OpenAI-compatible endpoint (prototype guard)"),
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
                            ),
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
