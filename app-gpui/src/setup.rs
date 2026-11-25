//! 3-step Setup Guide GPUI prototype matching `app/ui/setup.html` + `app/ui/setup.js`.

use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::theme::{ActiveTheme, ThemeMode};
use gpui_kit::component::{Disableable, Sizable, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Context, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Window, div, px,
};
use superviser_core::catalog::Role;

use crate::backend::Backend;
use crate::theme::{palette, whole_gib};
use crate::windows::{choose_project_folder, open_chat, open_settings};

const STEPS: [&str; 3] = ["Model", "Folder", "Start"];

pub struct SetupWindow {
    backend: Arc<Backend>,
    step: usize,
}

impl SetupWindow {
    pub fn new(backend: Arc<Backend>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut rx = backend.tx.subscribe();
        cx.spawn(async move |this, cx| {
            while rx.recv().await.is_ok() {
                let _ = this.update(cx, |_, cx| cx.notify());
            }
        })
        .detach();

        Self { backend, step: 0 }
    }
}

impl Render for SetupWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_dark = cx.theme().mode == ThemeMode::Dark;
        let p = palette(is_dark);

        let installed = self.backend.installed();
        let ready_model = installed
            .iter()
            .find(|m| self.backend.fit(&m.model).is_some())
            .map(|m| m.model.name.clone());
        let projects = self.backend.projects();
        let project = projects.first().cloned();
        let (picks, _, _) = self.backend.catalog_offers();

        let step_colors = [
            (p.green_1, p.on_green_1),
            (p.green_2, p.on_green_2),
            (p.green_3, p.on_green_3),
        ];

        let notes = [
            ready_model
                .clone()
                .unwrap_or_else(|| "Not chosen yet".into()),
            project
                .as_ref()
                .map(|pr| pr.name.clone())
                .unwrap_or_else(|| "Not chosen yet".into()),
            "Open the chat".into(),
        ];
        let done = [ready_model.is_some(), project.is_some(), false];

        let window_handle = window.window_handle();

        v_flex()
            .size_full()
            .bg(p.bg)
            .text_color(p.text)
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
                                    .label("Open Advanced Settings")
                                    .on_click(move |_, _, cx| {
                                        open_settings(None, cx);
                                        let _ = window_handle
                                            .update(cx, |_, window, _| window.remove_window());
                                    }),
                            ),
                    )
                    // 3 deepening-green step pills
                    .child(
                        h_flex()
                            .gap_2()
                            .children(STEPS.iter().enumerate().map(|(i, label)| {
                                let is_current = self.step == i;
                                let (step_bg, step_fg) = step_colors[i];
                                h_flex()
                                    .id(("step-pill", i))
                                    .flex_1()
                                    .min_w_0()
                                    .cursor_pointer()
                                    .px_3()
                                    .py_2()
                                    .gap_2()
                                    .items_center()
                                    .rounded(px(8.0))
                                    .border_1()
                                    .bg(if is_current { step_bg } else { p.panel })
                                    .border_color(if is_current { step_bg } else { p.line })
                                    .text_color(if is_current { step_fg } else { p.muted })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.step = i;
                                        cx.notify();
                                    }))
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
                            .when_some(ready_model.clone(), |col, name| {
                                col.child(
                                    div()
                                        .p_3()
                                        .rounded(px(8.0))
                                        .bg(p.chip)
                                        .text_color(p.good)
                                        .text_sm()
                                        .child(format!("✓ {name} is ready. Go on to the next step.")),
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
                                    .hardware
                                    .total_memory
                                    .saturating_sub(pick.offer.fit.required);
                                h_flex()
                                    .p_3()
                                    .gap_3()
                                    .items_center()
                                    .rounded(px(10.0))
                                    .bg(p.panel)
                                    .border_1()
                                    .border_color(if i == 0 { p.accent } else { p.line })
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
                                            .child(div().text_xs().text_color(p.muted).child(
                                                format!(
                                                    "{} · Leaves {} for your other apps",
                                                    if pick.offer.fast { "Quick replies" } else { "Slower replies" },
                                                    whole_gib(left)
                                                ),
                                            )),
                                    )
                                    .child(Tag::secondary().small().child(whole_gib(pick.offer.model.quant.size)))
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
                                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
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
                            v_flex()
                                .gap_3()
                                .child(
                                    div()
                                        .text_base()
                                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                        .child("Ready to test Chat"),
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
                                            "Guard & sandbox rules active",
                                            p,
                                        )),
                                )
                                .child(
                                    h_flex().mt_2().child(
                                        Button::new("start-chat")
                                            .primary()
                                            .label("Start chatting")
                                            .on_click(move |_, _, cx| {
                                                open_chat(proj_id.clone(), cx);
                                                let _ = window_handle
                                                    .update(cx, |_, window, _| window.remove_window());
                                            }),
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

impl SetupWindow {
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
