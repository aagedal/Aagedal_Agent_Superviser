//! 3-step Setup Guide wired to the real `superviser-backend` runtime:
//! pick/download a model, pick a project folder, optionally enable the judge, and start chatting.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::theme::{ActiveTheme, ThemeMode};
use gpui_kit::component::{Disableable, Sizable, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Context, FocusHandle, Focusable, InteractiveElement, IntoElement, KeyDownEvent,
    ParentElement, Render, StatefulInteractiveElement, Styled, Window, div, px,
};
use superviser_backend::{AppEvent, DownloadEvent, JUDGE_DOWNLOAD};
use superviser_core::catalog::Role;
use superviser_core::installed;

use crate::backend::Backend;
use crate::theme::{gib, palette, whole_gib};
use crate::windows::{AppGlobals, choose_project_folder, open_chat, open_settings};
use crate::{Dismiss, NextStep, PrevStep, SelectStep1, SelectStep2, SelectStep3};

const STEPS: [&str; 3] = ["Model", "Folder", "Start"];

pub struct SetupWindow {
    backend: Arc<Backend>,
    focus_handle: FocusHandle,
    step_focus_handles: [FocusHandle; 3],
    step: usize,
    chosen_model: Option<String>,
    downloads: HashMap<String, (DownloadEvent, Instant)>,
    error: Option<String>,
}

impl Focusable for SetupWindow {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl SetupWindow {
    pub fn new(backend: Arc<Backend>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let step_focus_handles = [cx.focus_handle(), cx.focus_handle(), cx.focus_handle()];
        focus_handle.focus(window, cx);

        let mut downloads = HashMap::new();
        for dl in backend.app.active_downloads() {
            downloads.insert(dl.id.clone(), (dl, Instant::now()));
        }

        let mut rx = backend.app.bus.subscribe();
        cx.spawn(async move |this, cx| {
            while let Some(event) = rx.recv().await {
                let _ = this.update(cx, |this, cx| match event {
                    AppEvent::Download(dl) => {
                        if dl.finished || dl.error.is_some() {
                            this.downloads.remove(&dl.id);
                            if let Some(err) = dl.error {
                                this.error = Some(format!("{}: {err}", dl.name));
                            } else if dl.id != JUDGE_DOWNLOAD && this.chosen_model.is_none() {
                                this.chosen_model = Some(dl.id);
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
                    AppEvent::Discovery
                    | AppEvent::Projects
                    | AppEvent::Status(_)
                    | AppEvent::LoadProgress { .. }
                    | AppEvent::Judge
                    | AppEvent::Reasoning => {
                        cx.notify();
                    }
                    _ => {}
                });
            }
        })
        .detach();

        Self {
            backend,
            focus_handle,
            step_focus_handles,
            step: 0,
            chosen_model: None,
            downloads,
            error: None,
        }
    }

    fn go_to_step(&mut self, step: usize, cx: &mut Context<Self>) {
        self.step = step.min(STEPS.len() - 1);
        cx.notify();
    }

    fn handle_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let mods = &event.keystroke.modifiers;
        if key == "enter" && (mods.platform || mods.control) {
            cx.stop_propagation();
            if self.step < 2 {
                self.go_to_step(self.step + 1, cx);
            } else {
                let projects = self.backend.app.settings.lock().unwrap().projects.clone();
                let proj_id = projects.first().map(|pr| pr.id.clone());
                let model_id = self.resolved_model().map(|(id, _, _)| id);
                let has_downloading = self.downloads.keys().any(|id| id != JUDGE_DOWNLOAD);
                if proj_id.is_some() && (model_id.is_some() || has_downloading) {
                    self.finish_setup("chat", proj_id, model_id, window, cx);
                }
            }
        }
    }

    fn resolved_model(&self) -> Option<(String, String, u64)> {
        let installed = self.backend.app.installed();
        if let Some(chosen) = &self.chosen_model
            && let Some(m) = installed.iter().find(|m| &m.id == chosen)
            && let Some(fit) = self.backend.app.fit(&m.model)
        {
            return Some((m.id.clone(), m.model.name.clone(), fit.required));
        }
        installed.iter().find_map(|m| {
            let fit = self.backend.app.fit(&m.model)?;
            Some((m.id.clone(), m.model.name.clone(), fit.required))
        })
    }

    fn finish_setup(
        &self,
        open_target: &'static str,
        project_id: Option<String>,
        model_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        {
            let mut settings = self.backend.app.settings.lock().unwrap();
            settings.setup_done = true;
        }
        if let Err(e) = self.backend.app.save_settings() {
            tracing::warn!("saving setup_done: {e:#}");
        }
        cx.global::<AppGlobals>().refresh_tray();

        // Start loading the chosen model in the background so Chat finds it loading immediately.
        if open_target == "chat"
            && let Some(mid) = model_id
            && !self.backend.app.loading_or_loaded(&mid)
        {
            let unload = self
                .backend
                .app
                .room_for(&mid)
                .map(|r| r.unload)
                .unwrap_or_default();
            self.backend.spawn_load_model(mid, unload);
        }

        match open_target {
            "chat" => open_chat(project_id, cx),
            _ => open_settings(None, cx),
        }
        window.remove_window();
    }

    fn render_downloads_banner(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if self.downloads.is_empty() && self.error.is_none() {
            return None;
        }
        let is_dark = cx.theme().mode == ThemeMode::Dark;
        let p = palette(is_dark);
        let mut active: Vec<(DownloadEvent, Instant)> = self.downloads.values().cloned().collect();
        active.sort_by(|a, b| a.0.name.cmp(&b.0.name));

        Some(
            v_flex()
                .gap_2()
                .children(active.into_iter().enumerate().map(|(i, (dl, started))| {
                    let pct = if dl.total > 0 {
                        ((dl.done as f64 / dl.total as f64) * 100.0).clamp(0.0, 100.0)
                    } else {
                        0.0
                    };
                    let eta = format_eta(dl.done, dl.total, started);
                    let dl_id = dl.id.clone();
                    v_flex()
                        .p_3()
                        .gap_2()
                        .rounded(px(8.0))
                        .bg(p.panel)
                        .border_1()
                        .border_color(p.accent)
                        .child(
                            h_flex()
                                .justify_between()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .text_xs()
                                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                        .child(format!(
                                            "Downloading {} — {} / {} ({:.0}%){eta}",
                                            dl.name,
                                            gib(dl.done),
                                            gib(dl.total),
                                            pct
                                        )),
                                )
                                .child(
                                    Button::new(("setup-cancel-dl", i))
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
                                .h(px(6.0))
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
                }))
                .when_some(self.error.clone(), |col, err| {
                    col.child(
                        h_flex()
                            .p_3()
                            .gap_2()
                            .items_center()
                            .justify_between()
                            .rounded(px(8.0))
                            .bg(p.bad.opacity(0.14))
                            .text_color(p.bad)
                            .text_xs()
                            .child(div().flex_1().child(err))
                            .child(
                                Button::new("dismiss-setup-err")
                                    .ghost()
                                    .small()
                                    .label("Dismiss")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.error = None;
                                        cx.notify();
                                    })),
                            ),
                    )
                }),
        )
    }

    fn checklist_row(
        done: bool,
        title: &str,
        detail: &str,
        p: crate::theme::Palette,
    ) -> impl IntoElement {
        h_flex()
            .p_3()
            .gap_3()
            .items_center()
            .rounded(px(8.0))
            .bg(p.panel)
            .border_1()
            .border_color(p.line)
            .child(
                h_flex()
                    .size(px(22.0))
                    .rounded_full()
                    .items_center()
                    .justify_center()
                    .text_xs()
                    .bg(if done {
                        p.good.opacity(0.18)
                    } else {
                        p.warn.opacity(0.18)
                    })
                    .text_color(if done { p.good } else { p.warn })
                    .child(if done { "✓" } else { "!" }),
            )
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .child(title.to_string()),
            )
            .child(div().text_sm().text_color(p.muted).child(detail.to_string()))
    }
}

impl Render for SetupWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_dark = cx.theme().mode == ThemeMode::Dark;
        let p = palette(is_dark);

        let installed = self.backend.app.installed();
        let ready = self.resolved_model();
        let projects = self.backend.app.settings.lock().unwrap().projects.clone();
        let project = projects.first().cloned();
        let offers = self
            .backend
            .app
            .catalog
            .offers(&self.backend.app.model_hardware());
        let picks = superviser_core::catalog::Catalog::picks(
            &offers,
            self.backend.app.hardware.total_memory,
        );

        let downloading_model = self
            .downloads
            .values()
            .find(|(d, _)| d.id != JUDGE_DOWNLOAD)
            .map(|(d, _)| d.name.clone());

        let step_colors = [
            (p.green_1, p.on_green_1),
            (p.green_2, p.on_green_2),
            (p.green_3, p.on_green_3),
        ];

        let notes = [
            ready
                .as_ref()
                .map(|(_, name, _)| name.clone())
                .or_else(|| downloading_model.as_ref().map(|n| format!("Downloading {n}…")))
                .unwrap_or_else(|| "Not chosen yet".into()),
            project
                .as_ref()
                .map(|pr| pr.name.clone())
                .unwrap_or_else(|| "Not chosen yet".into()),
            "Open the chat".into(),
        ];
        let done = [ready.is_some(), project.is_some(), false];

        let (judge_on, judge_fail_closed) = {
            let s = self.backend.app.settings.lock().unwrap();
            (s.judge, s.judge_fail_closed)
        };
        let judge_fits = self.backend.app.judge_fits();
        let judge_installed = self.backend.app.judge_installed().is_some();
        let judge_mem = self
            .backend
            .app
            .catalog
            .judge
            .as_ref()
            .map_or(0, |j| j.judge_required());
        let judge_warns = judge_fits
            && ready.as_ref().is_some_and(|(_, _, req)| {
                req + judge_mem > self.backend.app.hardware.model_budget
            });

        v_flex()
            .size_full()
            .bg(p.bg)
            .text_color(p.text)
            .track_focus(&self.focus_handle)
            .key_context("SetupWindow")
            .on_action(cx.listener(|this, _: &SelectStep1, _, cx| {
                this.go_to_step(0, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectStep2, _, cx| {
                this.go_to_step(1, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectStep3, _, cx| {
                this.go_to_step(2, cx);
            }))
            .on_action(cx.listener(|this, _: &PrevStep, _, cx| {
                this.go_to_step(this.step.saturating_sub(1), cx);
            }))
            .on_action(cx.listener(|this, _: &NextStep, _, cx| {
                this.go_to_step((this.step + 1).min(STEPS.len() - 1), cx);
            }))
            .on_action(cx.listener(|this, _: &Dismiss, _, cx| {
                if this.error.take().is_some() {
                    cx.notify();
                }
            }))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                this.handle_key_down(ev, window, cx);
            }))
            .child(
                div().flex_1().min_h_0().w_full().child(
                    div()
                        .id("setup-body")
                        .size_full()
                        .overflow_y_scrollbar()
                        .child(
                            v_flex()
                                .w_full()
                                .p_5()
                                .gap_4()
                                // Header
                                .child(
                                    h_flex()
                                        .justify_between()
                                        .items_baseline()
                                        .child(
                                            div()
                                                .text_lg()
                                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                                .child("Get started"),
                                        )
                                        .child(
                                            Button::new("skip-setup")
                                                .link()
                                                .small()
                                                .label("Skip setup and go to Advanced Settings")
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.finish_setup("settings", None, None, window, cx);
                                                })),
                                        ),
                                )
                                // Active downloads / error banner across all steps
                                .when_some(self.render_downloads_banner(cx), |col, banner| {
                                    col.child(banner)
                                })
                                // 3 deepening-green step pills
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .children(STEPS.iter().enumerate().map(|(i, label)| {
                                            let is_current = self.step == i;
                                            let (step_bg, step_fg) = step_colors[i];
                                            let pill_focus = self.step_focus_handles[i].clone();
                                            let is_focused = pill_focus.is_focused(window);
                                            h_flex()
                                                .id(("step-pill", i))
                                                .track_focus(&pill_focus)
                                                .tab_stop(true)
                                                .tab_index(0)
                                                .flex_1()
                                                .min_w_0()
                                                .cursor_pointer()
                                                .px_3()
                                                .py_2()
                                                .gap_2()
                                                .items_center()
                                                .rounded(px(8.0))
                                                .border_2()
                                                .bg(if is_current { step_bg } else { p.panel })
                                                .border_color(if is_focused {
                                                    p.accent
                                                } else if is_current {
                                                    step_bg
                                                } else {
                                                    p.line
                                                })
                                                .text_color(if is_current { step_fg } else { p.muted })
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.go_to_step(i, cx);
                                                }))
                                                .on_key_down(cx.listener(
                                                    move |this, ev: &KeyDownEvent, window, cx| {
                                                        let mods = &ev.keystroke.modifiers;
                                                        if mods.platform || mods.control || mods.alt {
                                                            return;
                                                        }
                                                        match ev.keystroke.key.as_str() {
                                                            "enter" | "space" => {
                                                                cx.stop_propagation();
                                                                this.go_to_step(i, cx);
                                                            }
                                                            "left" => {
                                                                cx.stop_propagation();
                                                                let prev = if i == 0 {
                                                                    STEPS.len() - 1
                                                                } else {
                                                                    i - 1
                                                                };
                                                                this.step_focus_handles[prev]
                                                                    .focus(window, cx);
                                                                this.go_to_step(prev, cx);
                                                            }
                                                            "right" => {
                                                                cx.stop_propagation();
                                                                let next = (i + 1) % STEPS.len();
                                                                this.step_focus_handles[next]
                                                                    .focus(window, cx);
                                                                this.go_to_step(next, cx);
                                                            }
                                                            "home" => {
                                                                cx.stop_propagation();
                                                                this.step_focus_handles[0]
                                                                    .focus(window, cx);
                                                                this.go_to_step(0, cx);
                                                            }
                                                            "end" => {
                                                                cx.stop_propagation();
                                                                let last = STEPS.len() - 1;
                                                                this.step_focus_handles[last]
                                                                    .focus(window, cx);
                                                                this.go_to_step(last, cx);
                                                            }
                                                            _ => {}
                                                        }
                                                    },
                                                ))
                                                .child(
                                                    h_flex()
                                                        .size(px(22.0))
                                                        .rounded_full()
                                                        .items_center()
                                                        .justify_center()
                                                        .text_xs()
                                                        .bg(if is_current {
                                                            step_fg
                                                        } else if done[i] {
                                                            step_bg
                                                        } else {
                                                            p.chip
                                                        })
                                                        .text_color(if is_current {
                                                            step_bg
                                                        } else if done[i] {
                                                            step_fg
                                                        } else {
                                                            p.muted
                                                        })
                                                        .child(if done[i] {
                                                            "✓".to_string()
                                                        } else {
                                                            (i + 1).to_string()
                                                        }),
                                                )
                                                .child(
                                                    v_flex()
                                                        .min_w_0()
                                                        .child(
                                                            div()
                                                                .text_sm()
                                                                .font_weight(gpui_kit::FontWeight::MEDIUM)
                                                                .child(*label),
                                                        )
                                                        .child(
                                                            div()
                                                                .text_xs()
                                                                .truncate()
                                                                .opacity(0.8)
                                                                .child(notes[i].clone()),
                                                        ),
                                                )
                                        })),
                                )
                                // Step content
                                .child(match self.step {
                                    0 => v_flex()
                                        .gap_3()
                                        .child(
                                            div()
                                                .text_base()
                                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                                .child("Pick a model"),
                                        )
                                        .when_some(ready.clone(), |col, (_, name, _)| {
                                            col.child(
                                                div()
                                                    .p_3()
                                                    .rounded(px(8.0))
                                                    .bg(p.chip)
                                                    .text_color(p.good)
                                                    .text_sm()
                                                    .child(format!(
                                                        "✓ {name} is ready. Go on to the next step."
                                                    )),
                                            )
                                        })
                                        .children(picks.iter().enumerate().map(|(i, pick)| {
                                            let role = match pick.role {
                                                Role::MostCapable => "Most capable",
                                                Role::Fast => "Capable and fast",
                                                Role::Light => "Lighter",
                                                Role::Basic => "For basic tasks",
                                            };
                                            let left = self
                                                .backend
                                                .app
                                                .hardware
                                                .total_memory
                                                .saturating_sub(pick.offer.fit.required);
                                            let local_match = installed.iter().find(|m| {
                                                m.model
                                                    .repo
                                                    .eq_ignore_ascii_case(&pick.offer.model.repo)
                                            });
                                            let catalog_id = installed::model_id(&pick.offer.model);
                                            let is_downloading =
                                                self.downloads.contains_key(&catalog_id);
                                            let is_selected = local_match.is_some_and(|m| {
                                                ready.as_ref().is_some_and(|(id, _, _)| id == &m.id)
                                            });
                                            let model_to_dl = pick.offer.model.clone();
                                            let local_id = local_match.map(|m| m.id.clone());

                                            h_flex()
                                                .p_3()
                                                .gap_3()
                                                .items_center()
                                                .rounded(px(10.0))
                                                .bg(p.panel)
                                                .border_1()
                                                .border_color(if i == 0 || is_selected {
                                                    p.accent
                                                } else {
                                                    p.line
                                                })
                                                .child(
                                                    v_flex()
                                                        .flex_1()
                                                        .gap_1()
                                                        .child(
                                                            h_flex()
                                                                .gap_2()
                                                                .items_baseline()
                                                                .child(
                                                                    div()
                                                                        .text_sm()
                                                                        .font_weight(
                                                                            gpui_kit::FontWeight::SEMIBOLD,
                                                                        )
                                                                        .child(role),
                                                                )
                                                                .child(
                                                                    div()
                                                                        .text_xs()
                                                                        .text_color(p.muted)
                                                                        .child(
                                                                            pick.offer
                                                                                .model
                                                                                .name
                                                                                .clone(),
                                                                        ),
                                                                ),
                                                        )
                                                        .child(
                                                            div()
                                                                .text_xs()
                                                                .text_color(p.muted)
                                                                .child(format!(
                                                                    "{} · Leaves {} for your other apps",
                                                                    if pick.offer.fast {
                                                                        "Quick replies"
                                                                    } else {
                                                                        "Slower replies"
                                                                    },
                                                                    whole_gib(left)
                                                                )),
                                                        )
                                                        .when(i == 0, |body| {
                                                            body.child(
                                                                h_flex().mt_1().child(
                                                                    Tag::primary()
                                                                        .small()
                                                                        .child("Good place to start"),
                                                                ),
                                                            )
                                                        }),
                                                )
                                                .child(if is_selected {
                                                    Tag::success()
                                                        .small()
                                                        .child("Ready ✓")
                                                        .into_any_element()
                                                } else if let Some(lid) = local_id {
                                                    Button::new(("setup-use", i))
                                                        .primary()
                                                        .small()
                                                        .label("Use this")
                                                        .on_click(cx.listener(
                                                            move |this, _, _, cx| {
                                                                this.chosen_model =
                                                                    Some(lid.clone());
                                                                cx.notify();
                                                            },
                                                        ))
                                                        .into_any_element()
                                                } else if is_downloading {
                                                    Tag::secondary()
                                                        .small()
                                                        .child("Downloading…")
                                                        .into_any_element()
                                                } else {
                                                    Button::new(("setup-dl", i))
                                                        .when(i == 0, |b| b.primary())
                                                        .small()
                                                        .label(format!(
                                                            "Download ({})",
                                                            whole_gib(pick.offer.model.quant.size)
                                                        ))
                                                        .on_click(cx.listener(
                                                            move |this, _, _, cx| {
                                                                this.error = None;
                                                                match this
                                                                    .backend
                                                                    .start_download_model(
                                                                        model_to_dl.clone(),
                                                                    ) {
                                                                    Ok(id) => {
                                                                        this.chosen_model =
                                                                            Some(id);
                                                                    }
                                                                    Err(e) => {
                                                                        this.error = Some(e);
                                                                    }
                                                                }
                                                                cx.notify();
                                                            },
                                                        ))
                                                        .into_any_element()
                                                })
                                        }))
                                        .into_any_element(),
                                    1 => v_flex()
                                        .gap_3()
                                        .child(
                                            div()
                                                .text_base()
                                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                                .child("Where should the agent work?"),
                                        )
                                        .child(div().text_sm().text_color(p.muted).child(
                                            "Pick the folder with your code. The agent can change files only in this folder and can't read your passwords or SSH keys.",
                                        ))
                                        .when_some(project.clone(), |col, pr| {
                                            col.child(
                                                v_flex()
                                                    .p_3()
                                                    .rounded(px(10.0))
                                                    .bg(p.panel)
                                                    .border_1()
                                                    .border_color(p.line)
                                                    .child(
                                                        div()
                                                            .text_sm()
                                                            .font_weight(
                                                                gpui_kit::FontWeight::SEMIBOLD,
                                                            )
                                                            .child(pr.name.clone()),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_xs()
                                                            .text_color(p.muted)
                                                            .child(pr.main().display().to_string()),
                                                    ),
                                            )
                                        })
                                        .child(
                                            h_flex().child(
                                                Button::new("choose-folder")
                                                    .primary()
                                                    .label(if project.is_some() {
                                                        "Choose another folder…"
                                                    } else {
                                                        "Choose folder…"
                                                    })
                                                    .on_click(|_, _, cx| {
                                                        choose_project_folder(
                                                            "Choose your project's main folder",
                                                            false,
                                                            cx,
                                                        );
                                                    }),
                                            ),
                                        )
                                        .into_any_element(),
                                    _ => {
                                        let proj_id = project.as_ref().map(|pr| pr.id.clone());
                                        let model_id = ready.as_ref().map(|(id, _, _)| id.clone());
                                        let can_start = proj_id.is_some()
                                            && (model_id.is_some() || downloading_model.is_some());
                                        v_flex()
                                            .gap_3()
                                            .child(
                                                div()
                                                    .text_base()
                                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                                    .child("Ready to start"),
                                            )
                                            .child(
                                                v_flex()
                                                    .gap_2()
                                                    .child(Self::checklist_row(
                                                        done[0],
                                                        "Model",
                                                        &notes[0],
                                                        p,
                                                    ))
                                                    .child(Self::checklist_row(
                                                        done[1],
                                                        "Folder",
                                                        &notes[1],
                                                        p,
                                                    ))
                                                    .child(Self::checklist_row(
                                                        true,
                                                        "Safety",
                                                        "Guard & macOS sandbox active on every tool call",
                                                        p,
                                                    )),
                                            )
                                            .when(judge_fits, |col| {
                                                col.child(
                                                    v_flex()
                                                        .p_3()
                                                        .gap_2()
                                                        .rounded(px(8.0))
                                                        .bg(p.panel)
                                                        .border_1()
                                                        .border_color(p.line)
                                                        .child(
                                                            Checkbox::new("setup-judge")
                                                                .checked(judge_on)
                                                                .label(format!(
                                                                    "Also run the safety check model (d3-mini, ~{}) on suspicious calls",
                                                                    gib(judge_mem)
                                                                ))
                                                                .on_click(cx.listener(
                                                                    move |this, checked, _, cx| {
                                                                        let enable = *checked;
                                                                        let backend = this.backend.clone();
                                                                        if enable
                                                                            && !judge_installed
                                                                            && let Err(e) = backend.start_download_judge()
                                                                        {
                                                                            this.error = Some(e);
                                                                            cx.notify();
                                                                            return;
                                                                        }
                                                                        let (tx, rx) = tokio::sync::oneshot::channel();
                                                                        let app = backend.app.clone();
                                                                        backend.tokio.spawn(async move {
                                                                            let res = app
                                                                                .set_judge(enable, judge_fail_closed)
                                                                                .await
                                                                                .map_err(|e| format!("{e:#}"));
                                                                            let _ = tx.send(res);
                                                                        });
                                                                        cx.spawn(async move |this, cx| {
                                                                            if let Ok(Err(e)) = rx.await {
                                                                                let _ = this.update(cx, |this, cx| {
                                                                                    this.error = Some(e);
                                                                                    cx.notify();
                                                                                });
                                                                            }
                                                                        })
                                                                        .detach();
                                                                    },
                                                                )),
                                                        )
                                                        .when(judge_warns, |box_| {
                                                            box_.child(
                                                                div()
                                                                    .text_xs()
                                                                    .text_color(p.warn)
                                                                    .child("Note: your chosen model + d3-mini exceed the memory budget together; pick a smaller model or leave the judge off."),
                                                            )
                                                        }),
                                                )
                                            })
                                            .child(
                                                h_flex().mt_2().child(
                                                    Button::new("start-chat")
                                                        .primary()
                                                        .disabled(!can_start)
                                                        .label("Start chatting")
                                                        .on_click(cx.listener(
                                                            move |this, _, window, cx| {
                                                                this.finish_setup(
                                                                    "chat",
                                                                    proj_id.clone(),
                                                                    model_id.clone(),
                                                                    window,
                                                                    cx,
                                                                );
                                                            },
                                                        )),
                                                ),
                                            )
                                            .into_any_element()
                                    }
                                }),
                        ),
                ),
            )
            // Sticky footer navigation
            .child(
                h_flex()
                    .flex_shrink_0()
                    .px_5()
                    .py_3()
                    .justify_between()
                    .bg(p.bg)
                    .border_t_1()
                    .border_color(p.line)
                    .child(
                        Button::new("setup-back")
                            .label("Back")
                            .disabled(self.step == 0)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.step = this.step.saturating_sub(1);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("setup-next")
                            .primary()
                            .label(match self.step {
                                0 => "Next: choose a folder",
                                1 => "Next: start",
                                _ => "Done",
                            })
                            .disabled(self.step >= 2)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.step = (this.step + 1).min(2);
                                cx.notify();
                            })),
                    ),
            )
    }
}

pub(crate) fn format_eta(done: u64, total: u64, started: Instant) -> String {
    let elapsed = started.elapsed().as_secs_f64();
    if elapsed < 2.0 || done == 0 || total <= done {
        return String::new();
    }
    let rate = done as f64 / elapsed;
    if rate <= 0.0 {
        return String::new();
    }
    let secs = ((total - done) as f64 / rate).round() as u64;
    if secs < 60 {
        format!(" · ~{secs}s left")
    } else {
        format!(" · ~{}m {}s left", secs / 60, secs % 60)
    }
}
