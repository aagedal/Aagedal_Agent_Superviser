//! Chat window GPUI prototype:
//! - Sidebar with real projects, chat list, and Settings/Setup links
//! - Scrollable message stream with Markdown (`TextView::markdown`, selectable + syntax highlighting),
//!   collapsible Thinking blocks, collapsible Tool call cards with +/- diffs & Allow/Deny approvals,
//!   and Guard blocked-call notices
//! - Multi-line composer (`Textarea`) with Enter-to-send / Shift+Enter newline and live streaming demo

use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::select::{SearchableVec, Select, SelectEvent, SelectItem, SelectState};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::text::TextView;
use gpui_kit::component::theme::{ActiveTheme, ThemeMode};
use gpui_kit::component::{Disableable, IndexPath, Sizable, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    ScrollHandle, SharedString, StatefulInteractiveElement, Styled, Subscription, Window, div, px,
};

use crate::backend::{Backend, BackendEvent};
use crate::theme::palette;
use crate::windows::{open_settings, open_setup};

#[derive(Clone, Debug, PartialEq)]
struct PickerOption {
    id: String,
    label: SharedString,
}

impl SelectItem for PickerOption {
    type Value = String;

    fn title(&self) -> SharedString {
        self.label.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.id
    }
}

#[derive(Clone)]
struct DiffLine {
    kind: DiffKind,
    text: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiffKind {
    Same,
    Del,
    Add,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ApprovalState {
    Pending,
    Allowed,
    Denied,
}

#[derive(Clone)]
enum ChatBlock {
    User(String),
    Thinking {
        id: usize,
        text: String,
        open: bool,
    },
    AssistantMarkdown {
        id: usize,
        text: String,
    },
    ToolCall {
        id: usize,
        verb: String,
        subject: String,
        status: String,
        open: bool,
        diff: Vec<DiffLine>,
        output: Option<String>,
        approval: Option<ApprovalState>,
    },
    GuardNote(String),
}

struct SessionEntry {
    id: String,
    title: String,
    blocks: Vec<ChatBlock>,
    usage_pct: Option<u8>,
}

const STARTERS: &[(&str, &str)] = &[
    (
        "Explain this project",
        "Explain what this project does and how its main files fit together. Don't change anything.",
    ),
    (
        "Look for bugs",
        "Look through this project for bugs. Tell me what you find and how you'd fix each one before changing anything.",
    ),
    (
        "Write a README",
        "Write a README.md that explains what this project is, how to set it up and how to run it.",
    ),
];

pub struct ChatWindow {
    backend: Arc<Backend>,
    selected_project: Option<String>,
    project_select: Entity<SelectState<SearchableVec<PickerOption>>>,
    model_select: Entity<SelectState<SearchableVec<PickerOption>>>,
    selected_model: Option<String>,
    sessions: Vec<SessionEntry>,
    active_session: Option<String>,
    composer: Entity<TextareaState>,
    scroll_handle: ScrollHandle,
    busy: bool,
    next_block_id: usize,
    _subscriptions: Vec<Subscription>,
}

impl ChatWindow {
    pub fn new(
        backend: Arc<Backend>,
        initial_project: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let projects = backend.projects();
        let selected_project = initial_project
            .filter(|id| projects.iter().any(|p| &p.id == id))
            .or_else(|| projects.first().map(|p| p.id.clone()));

        let project_options = Self::build_project_options(&backend);
        let project_idx = selected_project
            .as_deref()
            .and_then(|id| project_options.iter().position(|o| o.id == id))
            .map(IndexPath::new);
        let project_select = cx.new(|cx| {
            SelectState::new(SearchableVec::new(project_options), project_idx, window, cx)
        });

        let model_options = Self::build_model_options(&backend);
        let selected_model = backend
            .active_model_id()
            .or_else(|| model_options.first().map(|m| m.id.clone()));
        let model_idx = selected_model
            .as_deref()
            .and_then(|id| model_options.iter().position(|o| o.id == id))
            .map(IndexPath::new);
        let model_select = cx.new(|cx| {
            SelectState::new(SearchableVec::new(model_options), model_idx, window, cx)
        });

        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Ask the agent to do something… (Enter to send, Shift+Enter for a new line)")
                .rows(3)
                .auto_grow(2, 6)
        });

        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(
            &project_select,
            window,
            |this, _, event: &SelectEvent<SearchableVec<PickerOption>>, window, cx| {
                if let SelectEvent::Confirm(Some(project_id)) = event {
                    this.select_project(project_id.clone(), window, cx);
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &model_select,
            window,
            |this, _, event: &SelectEvent<SearchableVec<PickerOption>>, _window, cx| {
                if let SelectEvent::Confirm(Some(model_id)) = event {
                    this.selected_model = Some(model_id.clone());
                    cx.notify();
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &composer,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { shift: false, .. } = event {
                    this.send_current(window, cx);
                }
            },
        ));

        // Listen for backend updates (projects added, discovery finished, model toggled).
        let mut rx = backend.tx.subscribe();
        cx.spawn(async move |this, cx| {
            while let Ok(event) = rx.recv().await {
                let should_refresh = matches!(
                    event,
                    BackendEvent::ProjectsChanged
                        | BackendEvent::DiscoveryUpdated
                        | BackendEvent::ActiveModelChanged
                );
                if should_refresh {
                    let _ = this.update(cx, |this, cx| {
                        this.sync_from_backend(cx);
                    });
                }
            }
        })
        .detach();

        let mut sessions = vec![Self::demo_session()];
        if let Some(pid) = &selected_project {
            for summary in backend.saved_chats(pid) {
                sessions.push(SessionEntry {
                    id: summary.id,
                    title: summary.title,
                    blocks: Vec::new(),
                    usage_pct: None,
                });
            }
        }

        Self {
            backend,
            selected_project,
            project_select,
            model_select,
            selected_model,
            active_session: Some("demo-gpui".into()),
            sessions,
            composer,
            scroll_handle: ScrollHandle::new(),
            busy: false,
            next_block_id: 100,
            _subscriptions: subscriptions,
        }
    }

    fn demo_session() -> SessionEntry {
        SessionEntry {
            id: "demo-gpui".into(),
            title: "GPUI prototype tour".into(),
            usage_pct: Some(24),
            blocks: vec![
                ChatBlock::User(
                    "Show me how code blocks, diffs, approvals, and guard warnings feel in GPUI."
                        .into(),
                ),
                ChatBlock::Thinking {
                    id: 1,
                    open: false,
                    text: "Checking `crates/guard/src/proxy.rs` and `app-gpui/src/chat.rs`.\nI'll demonstrate selectable Markdown, Tree-sitter highlighting, a collapsible diff card with approval controls, and a guard block banner.".into(),
                },
                ChatBlock::AssistantMarkdown {
                    id: 2,
                    text: "### Native GPUI Chat Preview\n\nEverything here renders directly on Metal via **GPUI Kit** — no `WKWebView` process, no IPC serialization, and text inside this message is **drag-selectable**.\n\n```rust\n// Toggle Dock icon only while a window is open\nif cx.windows().is_empty() {\n    cx.set_activation_policy(ActivationPolicy::Accessory);\n} else {\n    cx.set_activation_policy(ActivationPolicy::Regular);\n    cx.activate(true);\n}\n```\n\nTry clicking the **Thinking** disclosure above, expanding the **Edit** diff below, clicking **Allow / Deny**, or typing a message in the composer to test live streaming.".into(),
                },
                ChatBlock::ToolCall {
                    id: 3,
                    verb: "Edit".into(),
                    subject: "app-gpui/src/windows.rs".into(),
                    status: "waiting for you".into(),
                    open: true,
                    diff: vec![
                        DiffLine { kind: DiffKind::Same, text: "pub fn sync_dock_policy(cx: &mut App) {".into() },
                        DiffLine { kind: DiffKind::Del, text: "    // Tauri: set_activation_policy via webview_windows()".into() },
                        DiffLine { kind: DiffKind::Add, text: "    let has_windows = !cx.windows().is_empty();".into() },
                        DiffLine { kind: DiffKind::Add, text: "    cx.set_activation_policy(if has_windows { Regular } else { Accessory });".into() },
                        DiffLine { kind: DiffKind::Same, text: "}".into() },
                    ],
                    output: None,
                    approval: Some(ApprovalState::Pending),
                },
                ChatBlock::GuardNote(
                    "The guard blocked a `bash` call: outbound host not mentioned in conversation."
                        .into(),
                ),
            ],
        }
    }

    fn build_project_options(backend: &Backend) -> Vec<PickerOption> {
        let projects = backend.projects();
        if projects.is_empty() {
            vec![PickerOption {
                id: String::new(),
                label: "No projects yet".into(),
            }]
        } else {
            projects
                .into_iter()
                .map(|p| PickerOption {
                    id: p.id,
                    label: p.name.into(),
                })
                .collect()
        }
    }

    fn build_model_options(backend: &Backend) -> Vec<PickerOption> {
        let active = backend.active_model_id();
        let installed: Vec<PickerOption> = backend
            .installed()
            .into_iter()
            .filter(|m| backend.fit(&m.model).is_some())
            .map(|m| {
                let bullet = if active.as_deref() == Some(m.id.as_str()) {
                    " ●"
                } else {
                    ""
                };
                PickerOption {
                    id: m.id,
                    label: format!("{}{}", m.model.name, bullet).into(),
                }
            })
            .collect();
        if installed.is_empty() {
            vec![PickerOption {
                id: "demo-model".into(),
                label: "Prototype Demo Model ●".into(),
            }]
        } else {
            installed
        }
    }

    fn sync_from_backend(&mut self, cx: &mut Context<Self>) {
        if let Some(active) = self.backend.active_model_id() {
            self.selected_model = Some(active);
        }
        cx.notify();
    }

    pub fn select_project(
        &mut self,
        project_id: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if project_id.is_empty() {
            return;
        }
        self.selected_project = Some(project_id.clone());
        let mut sessions = vec![Self::demo_session()];
        for summary in self.backend.saved_chats(&project_id) {
            sessions.push(SessionEntry {
                id: summary.id,
                title: summary.title,
                blocks: Vec::new(),
                usage_pct: None,
            });
        }
        self.sessions = sessions;
        self.active_session = Some("demo-gpui".into());
        cx.notify();
    }

    fn active_session_mut(&mut self) -> Option<&mut SessionEntry> {
        let id = self.active_session.as_deref()?;
        self.sessions.iter_mut().find(|s| s.id == id)
    }

    fn active_session_ref(&self) -> Option<&SessionEntry> {
        let id = self.active_session.as_deref()?;
        self.sessions.iter().find(|s| s.id == id)
    }

    fn next_id(&mut self) -> usize {
        self.next_block_id += 1;
        self.next_block_id
    }

    fn send_current(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let raw = self.composer.read(cx).value().to_string();
        let text = raw.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.composer
            .update(cx, |state, cx| state.set_value("", window, cx));

        // Create a session if we're on the empty "New chat" state.
        if self.active_session.is_none() {
            let new_id = format!("chat-{}", self.next_id());
            let title = text.chars().take(42).collect::<String>();
            self.sessions.insert(
                0,
                SessionEntry {
                    id: new_id.clone(),
                    title,
                    blocks: Vec::new(),
                    usage_pct: Some(6),
                },
            );
            self.active_session = Some(new_id);
        }

        let thought_id = self.next_id();
        let reply_id = self.next_id();
        let session_id = self.active_session.clone().unwrap();

        if let Some(session) = self.active_session_mut() {
            session.blocks.push(ChatBlock::User(text.clone()));
            session.blocks.push(ChatBlock::Thinking {
                id: thought_id,
                text: "Planning a response on GPUI's foreground executor…".into(),
                open: false,
            });
            session.blocks.push(ChatBlock::AssistantMarkdown {
                id: reply_id,
                text: String::new(),
            });
        }
        self.busy = true;
        self.scroll_handle.scroll_to_bottom();
        cx.notify();

        // Stream a realistic chunked response asynchronously to feel GPUI's streaming perf.
        let chunks: Vec<String> = vec![
            format!("Got it — you asked:\n\n> {text}\n\n"),
            "In the GPUI prototype, streamed chunks arrive over a Rust channel and update the ".into(),
            "`Entity<ChatWindow>` directly with **zero JSON/IPC round-trips**.\n\n".into(),
            "```rust\n// Direct in-process backend call — no #[tauri::command] glue\nlet offers = backend.catalog_offers();\n```\n\n".into(),
            "Next step if you like this feel: wire `app/src/acp.rs` and `app/src/agent.rs` directly into `BackendEvent`.".into(),
        ];

        cx.spawn(async move |this, cx| {
            for chunk in chunks {
                cx.background_executor()
                    .timer(Duration::from_millis(160))
                    .await;
                let still_running = this
                    .update(cx, |this, cx| {
                        if !this.busy {
                            return false;
                        }
                        if let Some(sess) = this.sessions.iter_mut().find(|s| s.id == session_id) {
                            for block in &mut sess.blocks {
                                if let ChatBlock::AssistantMarkdown { id, text } = block
                                    && *id == reply_id
                                {
                                    text.push_str(&chunk);
                                }
                            }
                        }
                        this.scroll_handle.scroll_to_bottom();
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !still_running {
                    break;
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                cx.notify();
            });
        })
        .detach();
    }

    fn toggle_block_open(&mut self, block_id: usize, cx: &mut Context<Self>) {
        if let Some(session) = self.active_session_mut() {
            for block in &mut session.blocks {
                match block {
                    ChatBlock::Thinking { id, open, .. } if *id == block_id => *open = !*open,
                    ChatBlock::ToolCall { id, open, .. } if *id == block_id => *open = !*open,
                    _ => {}
                }
            }
        }
        cx.notify();
    }

    fn answer_approval(&mut self, block_id: usize, allow: bool, cx: &mut Context<Self>) {
        if let Some(session) = self.active_session_mut() {
            for block in &mut session.blocks {
                if let ChatBlock::ToolCall {
                    id,
                    status,
                    approval,
                    output,
                    ..
                } = block
                    && *id == block_id
                {
                    *approval = Some(if allow {
                        ApprovalState::Allowed
                    } else {
                        ApprovalState::Denied
                    });
                    *status = if allow {
                        "completed".into()
                    } else {
                        "declined".into()
                    };
                    if allow {
                        *output = Some("Applied edit cleanly (prototype).".into());
                    }
                }
            }
        }
        cx.notify();
    }

    fn render_empty_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_dark = cx.theme().mode == ThemeMode::Dark;
        let p = palette(is_dark);
        let project_name = self
            .selected_project
            .as_deref()
            .and_then(|id| self.backend.projects().into_iter().find(|pr| pr.id == id))
            .map(|pr| pr.name)
            .unwrap_or_else(|| "your project".into());

        v_flex()
            .mt(px(72.0))
            .mx_auto()
            .max_w(px(480.0))
            .items_center()
            .gap_3()
            .child(
                div()
                    .text_base()
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .child(format!("What should the agent do in {project_name}?")),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(p.muted)
                    .text_center()
                    .child("It can read and change files in your project folder and run commands in a sandbox. It can't see your passwords, keys, or other folders."),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .justify_center()
                    .gap_2()
                    .mt_2()
                    .children(STARTERS.iter().enumerate().map(|(i, (label, prompt))| {
                        let prompt = (*prompt).to_string();
                        Button::new(("starter", i))
                            .label(*label)
                            .small()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.composer.update(cx, |state, cx| {
                                    state.set_value(prompt.clone(), window, cx);
                                });
                            }))
                    })),
            )
    }

    fn render_block(&self, block: &ChatBlock, cx: &mut Context<Self>) -> impl IntoElement {
        let is_dark = cx.theme().mode == ThemeMode::Dark;
        let p = palette(is_dark);

        match block {
            ChatBlock::User(text) => h_flex().w_full().flex_shrink_0().justify_end().child(
                div()
                    .max_w(px(560.0))
                    .px_3()
                    .py_2()
                    .rounded(px(10.0))
                    .bg(p.accent)
                    .text_color(p.accent_text)
                    .text_sm()
                    .child(text.clone()),
            ),
            ChatBlock::Thinking { id, text, open } => {
                let block_id = *id;
                let arrow = if *open { "▾" } else { "▸" };
                v_flex()
                    .w_full()
                    .flex_shrink_0()
                    .gap_1()
                    .child(
                        div()
                            .id(("thinking-toggle", block_id))
                            .cursor_pointer()
                            .text_xs()
                            .text_color(p.muted)
                            .child(format!("{arrow} Thinking"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.toggle_block_open(block_id, cx);
                            })),
                    )
                    .when(*open, |this| {
                        this.child(
                            div()
                                .pl_3()
                                .py_1()
                                .border_l_2()
                                .border_color(p.line)
                                .text_xs()
                                .text_color(p.muted)
                                .child(text.clone()),
                        )
                    })
            }
            ChatBlock::AssistantMarkdown { id, text } => div()
                .w_full()
                .flex_shrink_0()
                .text_sm()
                .child(TextView::markdown(("md-msg", *id), text.clone()).selectable(true)),
            ChatBlock::ToolCall {
                id,
                verb,
                subject,
                status,
                open,
                diff,
                output,
                approval,
            } => {
                let block_id = *id;
                let arrow = if *open { "▾" } else { "▸" };
                let state_color = match status.as_str() {
                    "failed" => p.bad,
                    "declined" | "waiting for you" => p.warn,
                    _ => p.muted,
                };
                v_flex()
                    .w_full()
                    .flex_shrink_0()
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(p.line)
                    .bg(p.panel)
                    .child(
                        h_flex()
                            .id(("tool-head", block_id))
                            .cursor_pointer()
                            .px_3()
                            .py_2()
                            .gap_2()
                            .items_center()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.toggle_block_open(block_id, cx);
                            }))
                            .child(div().text_xs().text_color(p.muted).child(arrow))
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .child(verb.clone()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .text_xs()
                                    .text_color(p.accent)
                                    .truncate()
                                    .child(subject.clone()),
                            )
                            .child(div().text_xs().text_color(state_color).child(status.clone())),
                    )
                    .when(*open, |card| {
                        card.child(
                            v_flex()
                                .px_3()
                                .pb_2()
                                .gap_2()
                                .when(!diff.is_empty(), |body| {
                                    body.child(
                                        div()
                                            .id(("tool-diff", block_id))
                                            .max_h(px(320.0))
                                            .overflow_y_scroll()
                                            .rounded(px(6.0))
                                            .border_1()
                                            .border_color(p.line)
                                            .bg(p.bg)
                                            .child(v_flex().w_full().children(diff.iter().map(
                                                |line| {
                                                    let (prefix, bg, fg) = match line.kind {
                                                        DiffKind::Same => (" ", p.bg, p.muted),
                                                        DiffKind::Del => {
                                                            ("-", p.bad.opacity(0.14), p.text)
                                                        }
                                                        DiffKind::Add => {
                                                            ("+", p.good.opacity(0.14), p.text)
                                                        }
                                                    };
                                                    div()
                                                        .flex_shrink_0()
                                                        .px_2()
                                                        .py(px(2.0))
                                                        .bg(bg)
                                                        .text_color(fg)
                                                        .text_xs()
                                                        .child(format!("{prefix} {}", line.text))
                                                },
                                            ))),
                                    )
                                })
                                .when_some(output.clone(), |body, out| {
                                    body.child(
                                        div()
                                            .px_2()
                                            .py_1()
                                            .rounded(px(6.0))
                                            .bg(p.chip)
                                            .text_xs()
                                            .text_color(p.muted)
                                            .child(out),
                                    )
                                })
                                .when(matches!(approval, Some(ApprovalState::Pending)), |body| {
                                    body.child(
                                        h_flex()
                                            .px_3()
                                            .py_2()
                                            .rounded(px(6.0))
                                            .bg(p.chip)
                                            .gap_2()
                                            .items_center()
                                            .child(
                                                v_flex()
                                                    .flex_1()
                                                    .child(
                                                        div()
                                                            .text_sm()
                                                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                                            .child("Make this change?"),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_xs()
                                                            .text_color(p.muted)
                                                            .child("Only files in the project can change, and the guard has checked it."),
                                                    ),
                                            )
                                            .child(
                                                Button::new(("allow", block_id))
                                                    .primary()
                                                    .small()
                                                    .label("Allow")
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        this.answer_approval(block_id, true, cx);
                                                    })),
                                            )
                                            .child(
                                                Button::new(("deny", block_id))
                                                    .small()
                                                    .label("Deny")
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        this.answer_approval(block_id, false, cx);
                                                    })),
                                            ),
                                    )
                                }),
                        )
                    })
            }
            ChatBlock::GuardNote(note) => div()
                .w_full()
                .flex_shrink_0()
                .px_3()
                .py_2()
                .rounded(px(6.0))
                .bg(p.warn.opacity(0.14))
                .text_color(p.warn)
                .text_xs()
                .child(note.clone()),
        }
    }
}

impl Render for ChatWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_dark = cx.theme().mode == ThemeMode::Dark;
        let p = palette(is_dark);

        let project_path = self
            .selected_project
            .as_deref()
            .and_then(|id| self.backend.projects().into_iter().find(|pr| pr.id == id))
            .map(|pr| format!("· {}", pr.main().display()))
            .unwrap_or_else(|| "· Prototype workspace".into());

        let active_title = self
            .active_session_ref()
            .map(|s| s.title.clone())
            .unwrap_or_else(|| "New chat".into());
        let usage_text = self
            .active_session_ref()
            .and_then(|s| s.usage_pct)
            .map(|pct| format!("Chat {pct}% full"));

        let active_blocks: Vec<ChatBlock> = self
            .active_session_ref()
            .map(|s| s.blocks.clone())
            .unwrap_or_default();
        let is_empty = active_blocks.is_empty();
        let rendered_blocks: Vec<gpui_kit::AnyElement> = active_blocks
            .iter()
            .map(|block| self.render_block(block, cx).into_any_element())
            .collect();

        h_flex()
            .size_full()
            .bg(p.bg)
            .text_color(p.text)
            // Sidebar
            .child(
                v_flex()
                    .w(px(236.0))
                    .flex_shrink_0()
                    .h_full()
                    .p_3()
                    .gap_2()
                    .bg(p.panel)
                    .border_r_1()
                    .border_color(p.line)
                    .child(
                        div()
                            .w_full()
                            .h(px(30.0))
                            .flex_shrink_0()
                            .child(Select::new(&self.project_select).small()),
                    )
                    .child(
                        div().w_full().flex_shrink_0().child(
                            Button::new("new-chat")
                                .primary()
                                .w_full()
                                .label("New chat")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.active_session = None;
                                    cx.notify();
                                })),
                        ),
                    )
                    .child(
                        div()
                            .id("chat-list")
                            .flex_1()
                            .min_h_0()
                            .w_full()
                            .overflow_y_scroll()
                            .mt_1()
                            .child(v_flex().w_full().gap_1().children(
                                self.sessions.iter().enumerate().map(|(i, s)| {
                                    let sid = s.id.clone();
                                    let active =
                                        self.active_session.as_deref() == Some(s.id.as_str());
                                    div()
                                        .id(("session-row", i))
                                        .flex_shrink_0()
                                        .cursor_pointer()
                                        .px_2()
                                        .py(px(6.0))
                                        .rounded(px(6.0))
                                        .text_sm()
                                        .truncate()
                                        .when(active, |d| {
                                            d.bg(p.chip)
                                                .font_weight(gpui_kit::FontWeight::MEDIUM)
                                        })
                                        .hover(|d| d.bg(p.chip))
                                        .child(s.title.clone())
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.active_session = Some(sid.clone());
                                            cx.notify();
                                        }))
                                }),
                            )),
                    )
                    .child(
                        v_flex()
                            .flex_shrink_0()
                            .pt_2()
                            .gap_1()
                            .border_t_1()
                            .border_color(p.line)
                            .child(
                                Button::new("open-settings")
                                    .link()
                                    .small()
                                    .label("Projects and settings…")
                                    .on_click(|_, _, cx| open_settings(None, cx)),
                            )
                            .child(
                                Button::new("open-setup")
                                    .link()
                                    .small()
                                    .label("Setup guide…")
                                    .on_click(|_, _, cx| open_setup(cx)),
                            ),
                    ),
            )
            // Main chat column
            .child(
                v_flex()
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .min_h_0()
                    // Header
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .px_4()
                            .py_2()
                            .gap_3()
                            .items_center()
                            .justify_between()
                            .bg(p.panel)
                            .border_b_1()
                            .border_color(p.line)
                            .child(
                                h_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_2()
                                    .items_baseline()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                            .child(active_title),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(p.muted)
                                            .truncate()
                                            .child(project_path),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .flex_shrink_0()
                                    .gap_2()
                                    .items_center()
                                    .when_some(usage_text, |row, usage| {
                                        row.child(Tag::secondary().small().child(usage))
                                    })
                                    .child(
                                        div()
                                            .w(px(200.0))
                                            .h(px(28.0))
                                            .flex_shrink_0()
                                            .child(Select::new(&self.model_select).small()),
                                    ),
                            ),
                    )
                    // Message stream (relative wrapper + vertical scrollbar + non-shrinking inner column)
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_h_0()
                            .w_full()
                            .child(
                                div()
                                    .id("chat-messages")
                                    .track_scroll(&self.scroll_handle)
                                    .size_full()
                                    .overflow_y_scroll()
                                    .child(
                                        v_flex()
                                            .w_full()
                                            .px_5()
                                            .py_4()
                                            .gap_3()
                                            .when(is_empty, |box_| {
                                                box_.child(self.render_empty_state(cx))
                                            })
                                            .children(rendered_blocks),
                                    ),
                            )
                            .vertical_scrollbar(&self.scroll_handle),
                    )
                    // Working status line
                    .when(self.busy, |col| {
                        col.child(
                            h_flex()
                                .flex_shrink_0()
                                .px_5()
                                .py_1()
                                .gap_2()
                                .items_center()
                                .child(div().size(px(8.0)).rounded_full().bg(p.accent))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(p.muted)
                                        .child("Streaming response…"),
                                ),
                        )
                    })
                    // Composer
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .px_4()
                            .py_3()
                            .gap_2()
                            .items_end()
                            .bg(p.panel)
                            .border_t_1()
                            .border_color(p.line)
                            .child(div().flex_1().child(Textarea::new(&self.composer)))
                            .child(if self.busy {
                                Button::new("stop-btn")
                                    .danger()
                                    .label("Stop")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.busy = false;
                                        cx.notify();
                                    }))
                            } else {
                                Button::new("send-btn")
                                    .primary()
                                    .label("Send")
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.send_current(window, cx);
                                    }))
                            }),
                    ),
            )
    }
}
