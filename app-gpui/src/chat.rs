//! Native GPUI Chat window wired end-to-end to `superviser-backend`:
//! - OpenCode ACP (`opencode acp`) when available, with automatic fallback to the built-in agent
//! - Real session list, replay, archiving, deletion, and per-chat model persistence
//! - Live streaming Markdown (`TextView::markdown`), collapsible Thinking & Tool call cards,
//!   +/- diffs, real Allow/Allow-for-chat/Deny permission prompts, and Guard block notices
//! - Automatic model loading & room-check confirmation before sending, plus context-usage warning

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::select::{SearchableVec, Select, SelectEvent, SelectItem, SelectState};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::text::TextView;
use gpui_kit::component::theme::{ActiveTheme, ThemeMode};
use gpui_kit::component::{Disableable, IndexPath, Selectable, Sizable, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Anchor, AppContext, Context, Div, Entity, Hsla, InteractiveElement, IntoElement,
    ParentElement, Pixels, Render, ScrollHandle, SharedString, Stateful,
    StatefulInteractiveElement, Styled, Subscription, Window, div, px,
};
use serde_json::Value;
use superviser_backend::explain::{self, TokenKind};
use superviser_backend::skills::{self, Skill, SkillScope};
use superviser_backend::{AcpEvent, AppEvent, ChatEvent, JudgeStatus, LoadState, ReasoningLevel};
use superviser_core::agent::Chat;
use superviser_core::opencode;
use superviser_core::project::Approvals;
use superviser_core::sandbox::Isolation;

use crate::backend::Backend;
use crate::theme::palette;
use crate::windows::{choose_project_folder, open_settings, open_setup};

/// Share of context window from which we suggest archiving and starting a new chat.
const LONG_CHAT_RATIO: f64 = 0.6;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChatEngine {
    OpenCode,
    Builtin,
}

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
struct SessionMeta {
    id: String,
    title: String,
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

#[derive(Clone)]
struct PermissionOption {
    option_id: String,
    kind: String,
    label: String,
}

#[derive(Clone)]
struct PendingApproval {
    request_id: String,
    options: Vec<PermissionOption>,
}

#[derive(Clone)]
struct ToolCallBlock {
    id: usize,
    call_id: String,
    kind: String,
    verb: String,
    subject: String,
    status: String,
    open: bool,
    diff: Vec<DiffLine>,
    output: Option<String>,
    approval: Option<PendingApproval>,
    raw: Value,
}

#[derive(Clone)]
enum ChatBlock {
    User {
        key: Option<String>,
        text: String,
    },
    Thinking {
        id: usize,
        key: String,
        text: String,
        open: bool,
    },
    Assistant {
        id: usize,
        key: String,
        raw_text: String,
        text: String,
        guard_notes: Vec<String>,
    },
    ToolCall(Box<ToolCallBlock>),
    StatusNote {
        text: String,
        is_error: bool,
    },
}

struct PendingSend {
    display_text: String,
    prompt_text: String,
    model_id: String,
    model_name: String,
    unload_ids: Vec<String>,
    unload_names: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WorkingPhase {
    StartingAgent,
    ReadingPrompt,
    Thinking,
    Writing,
    RunningTool,
}

struct SkillEditor {
    scope: SkillScope,
    name_input: Entity<InputState>,
    desc_input: Entity<InputState>,
    body_input: Entity<TextareaState>,
    editing_original: Option<Skill>,
}

#[derive(Clone)]
enum ModelExplainState {
    Loading { model_name: String },
    Ready { model_name: String, text: String },
    Error(String),
}

struct CommandTokenTrigger {
    id: usize,
    text: String,
    space_left: Pixels,
    text_color: Hsla,
    underline_color: Hsla,
    hover_bg: Hsla,
    selected: bool,
}

impl Selectable for CommandTokenTrigger {
    fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    fn is_selected(&self) -> bool {
        self.selected
    }
}

impl IntoElement for CommandTokenTrigger {
    type Element = Stateful<Div>;

    fn into_element(self) -> Self::Element {
        let hover_bg = self.hover_bg;
        div()
            .id(("cmd-tok-trig", self.id))
            .ml(self.space_left)
            .px(px(2.0))
            .pb(px(1.0))
            .rounded(px(3.0))
            .cursor_pointer()
            .border_b_1()
            .border_dashed()
            .border_color(self.underline_color)
            .when(self.selected, move |d| d.bg(hover_bg))
            .hover(move |d| d.bg(hover_bg))
            .text_xs()
            .text_color(self.text_color)
            .child(self.text)
    }
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
    (
        "Start something new",
        "If this folder is empty, make a simple web page in index.html with a title, a short introduction and a contact link, in plain HTML and CSS. If it already has files, tell me what's here first and don't change anything.",
    ),
];

pub struct ChatWindow {
    backend: Arc<Backend>,
    engine: ChatEngine,
    selected_project: Option<String>,
    project_select: Entity<SelectState<SearchableVec<PickerOption>>>,
    project_sub: Subscription,
    model_select: Entity<SelectState<SearchableVec<PickerOption>>>,
    model_sub: Subscription,
    reasoning_select: Entity<SelectState<SearchableVec<PickerOption>>>,
    reasoning_sub: Subscription,
    selected_model: Option<String>,
    sessions: Vec<SessionMeta>,
    archived: HashSet<String>,
    show_archived: bool,
    active_session: Option<String>,
    blocks: Vec<ChatBlock>,
    usage: Option<(u64, u64)>, // (used, size)
    starting_note: Option<String>,
    sessions_error: Option<String>,
    pending_send: Option<PendingSend>,
    confirm_delete_chat: Option<(String, String)>, // (id, title)
    composer: Entity<TextareaState>,
    scroll_handle: ScrollHandle,
    auto_scroll: bool,
    last_offset_y: Pixels,
    busy: bool,
    working_since: Option<Instant>,
    working_phase: WorkingPhase,
    working_since_by_session: HashMap<String, Instant>,
    working_phase_by_session: HashMap<String, WorkingPhase>,
    creating_session: bool,
    buffered_events: Vec<AppEvent>,
    chat_gen: u64,
    list_gen: u64,
    send_gen: u64,
    next_block_id: usize,
    model_explanations: HashMap<String, ModelExplainState>,
    skills: Vec<Skill>,
    active_skill: Option<Skill>,
    show_slash_menu: bool,
    show_skills_panel: bool,
    skill_editor: Option<SkillEditor>,
    skill_drafting: bool,
    skill_notice: Option<(String, bool)>,
    _composer_sub: Subscription,
}

impl ChatWindow {
    pub fn new(
        backend: Arc<Backend>,
        initial_project: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let engine = if opencode::binary().is_some() {
            ChatEngine::OpenCode
        } else {
            ChatEngine::Builtin
        };

        let projects = backend.app.settings.lock().unwrap().projects.clone();
        let selected_project = initial_project
            .filter(|id| projects.iter().any(|p| &p.id == id))
            .or_else(|| projects.first().map(|p| p.id.clone()));

        let (project_select, project_sub) =
            Self::make_project_select(&backend, selected_project.as_deref(), window, cx);

        let selected_model = Self::default_model(&backend);
        let (model_select, model_sub) =
            Self::make_model_select(&backend, selected_model.as_deref(), window, cx);
        let (reasoning_select, reasoning_sub) =
            Self::make_reasoning_select(&backend, selected_model.as_deref(), window, cx);

        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(
                    "Ask the agent to do something… Type / for skills (Enter to send, Shift+Enter for a new line)",
                )
                .rows(3)
                .auto_grow(2, 6)
        });

        let composer_sub = cx.subscribe_in(
            &composer,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { shift: false, .. } => {
                    this.send_current(window, cx);
                }
                InputEvent::Change => {
                    this.show_slash_menu = false;
                    cx.notify();
                }
                _ => {}
            },
        );

        // Listen for all backend events (ACP, Built-in Chat, Model status, Projects, Discovery).
        let mut rx = backend.app.bus.subscribe();
        cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = rx.recv().await {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.handle_app_event(event, window, cx);
                });
            }
        })
        .detach();

        // 1-second tick for the elapsed "Working… Xs" timer while busy.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let alive = this
                    .update(cx, |this, cx| {
                        if this.busy {
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();

        let mut this = Self {
            backend,
            engine,
            selected_project: selected_project.clone(),
            project_select,
            project_sub,
            model_select,
            model_sub,
            reasoning_select,
            reasoning_sub,
            selected_model,
            sessions: Vec::new(),
            archived: HashSet::new(),
            show_archived: false,
            active_session: None,
            blocks: Vec::new(),
            usage: None,
            starting_note: None,
            sessions_error: None,
            pending_send: None,
            confirm_delete_chat: None,
            composer,
            scroll_handle: ScrollHandle::new(),
            auto_scroll: true,
            last_offset_y: px(0.0),
            busy: false,
            working_since: None,
            working_phase: WorkingPhase::ReadingPrompt,
            working_since_by_session: HashMap::new(),
            working_phase_by_session: HashMap::new(),
            creating_session: false,
            buffered_events: Vec::new(),
            chat_gen: 0,
            list_gen: 0,
            send_gen: 0,
            next_block_id: 1,
            model_explanations: HashMap::new(),
            skills: Vec::new(),
            active_skill: None,
            show_slash_menu: false,
            show_skills_panel: false,
            skill_editor: None,
            skill_drafting: false,
            skill_notice: None,
            _composer_sub: composer_sub,
        };

        this.refresh_skills();
        if selected_project.is_some() {
            this.refresh_sessions(cx);
        }
        this
    }

    fn make_project_select(
        backend: &Backend,
        selected_id: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (
        Entity<SelectState<SearchableVec<PickerOption>>>,
        Subscription,
    ) {
        let projects = backend.app.settings.lock().unwrap().projects.clone();
        let options: Vec<PickerOption> = if projects.is_empty() {
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
        };
        let idx = selected_id
            .and_then(|id| options.iter().position(|o| o.id == id))
            .map(IndexPath::new);
        let select = cx.new(|cx| SelectState::new(SearchableVec::new(options), idx, window, cx));
        let sub = cx.subscribe_in(
            &select,
            window,
            |this, _, event: &SelectEvent<SearchableVec<PickerOption>>, window, cx| {
                if let SelectEvent::Confirm(Some(project_id)) = event {
                    this.select_project(project_id.clone(), window, cx);
                }
            },
        );
        (select, sub)
    }

    fn make_model_select(
        backend: &Backend,
        selected_id: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (
        Entity<SelectState<SearchableVec<PickerOption>>>,
        Subscription,
    ) {
        let loaded = backend.app.models();
        let installed: Vec<PickerOption> = backend
            .app
            .installed()
            .into_iter()
            .filter(|m| backend.app.fit(&m.model).is_some())
            .map(|m| {
                let marker = match loaded.iter().find(|l| l.id == m.id).map(|l| &l.state) {
                    Some(LoadState::Running { .. }) => " ●",
                    Some(LoadState::Loading { .. }) => " …",
                    _ => "",
                };
                PickerOption {
                    id: m.id,
                    label: format!("{}{}", m.model.name, marker).into(),
                }
            })
            .collect();
        let options = if installed.is_empty() {
            vec![PickerOption {
                id: String::new(),
                label: "No models yet".into(),
            }]
        } else {
            installed
        };
        let idx = selected_id
            .and_then(|id| options.iter().position(|o| o.id == id))
            .map(IndexPath::new);
        let select = cx.new(|cx| SelectState::new(SearchableVec::new(options), idx, window, cx));
        let sub = cx.subscribe_in(
            &select,
            window,
            |this, _, event: &SelectEvent<SearchableVec<PickerOption>>, window, cx| {
                if let SelectEvent::Confirm(Some(model_id)) = event
                    && !model_id.is_empty()
                {
                    this.selected_model = Some(model_id.clone());
                    if let (Some(pid), Some(sid)) = (&this.selected_project, &this.active_session) {
                        let _ = this.backend.app.set_chat_model(pid, sid, Some(model_id.clone()));
                    }
                    this.rebuild_reasoning_select(window, cx);
                    cx.notify();
                }
            },
        );
        (select, sub)
    }

    fn make_reasoning_select(
        backend: &Backend,
        selected_model: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (
        Entity<SelectState<SearchableVec<PickerOption>>>,
        Subscription,
    ) {
        let options: Vec<PickerOption> = ReasoningLevel::ALL
            .iter()
            .map(|level| PickerOption {
                id: reasoning_key(*level).to_string(),
                label: level.short_label().into(),
            })
            .collect();
        let current = selected_model
            .map(|id| backend.app.model_reasoning(id))
            .unwrap_or_default();
        let idx = ReasoningLevel::ALL
            .iter()
            .position(|&l| l == current)
            .map(IndexPath::new);
        let select = cx.new(|cx| SelectState::new(SearchableVec::new(options), idx, window, cx));
        let sub = cx.subscribe_in(
            &select,
            window,
            |this, _, event: &SelectEvent<SearchableVec<PickerOption>>, _window, cx| {
                if let SelectEvent::Confirm(Some(key)) = event
                    && let Some(model_id) = this.selected_model.clone()
                {
                    let level = parse_reasoning_key(key);
                    if let Err(e) = this.backend.app.set_model_reasoning(&model_id, level) {
                        tracing::warn!("saving reasoning level for {model_id}: {e:#}");
                    }
                    cx.notify();
                }
            },
        );
        (select, sub)
    }

    fn rebuild_reasoning_select(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (select, sub) = Self::make_reasoning_select(
            &self.backend,
            self.selected_model.as_deref(),
            window,
            cx,
        );
        self.reasoning_select = select;
        self.reasoning_sub = sub;
    }

    fn default_model(backend: &Backend) -> Option<String> {
        let fit_ids: Vec<String> = backend
            .app
            .installed()
            .into_iter()
            .filter(|m| backend.app.fit(&m.model).is_some())
            .map(|m| m.id)
            .collect();
        if let Some(running) = backend
            .app
            .models()
            .into_iter()
            .find(|m| !matches!(m.state, LoadState::Failed { .. }) && fit_ids.contains(&m.id))
        {
            return Some(running.id);
        }
        if let Some(last) = backend.app.settings.lock().unwrap().last_model.clone()
            && fit_ids.contains(&last)
        {
            return Some(last);
        }
        fit_ids.into_iter().next()
    }

    fn rebuild_model_select(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let valid_models: Vec<String> = self
            .backend
            .app
            .installed()
            .into_iter()
            .filter(|m| self.backend.app.fit(&m.model).is_some())
            .map(|m| m.id)
            .collect();
        if self
            .selected_model
            .as_ref()
            .is_none_or(|id| !valid_models.contains(id))
        {
            self.selected_model = Self::default_model(&self.backend);
        }
        let (select, sub) =
            Self::make_model_select(&self.backend, self.selected_model.as_deref(), window, cx);
        self.model_select = select;
        self.model_sub = sub;
        self.rebuild_reasoning_select(window, cx);
    }

    fn rebuild_project_select(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let projects = self.backend.app.settings.lock().unwrap().projects.clone();
        let prev = self.selected_project.clone();
        if self
            .selected_project
            .as_ref()
            .is_none_or(|id| !projects.iter().any(|p| &p.id == id))
        {
            self.selected_project = projects.first().map(|p| p.id.clone());
        }
        let (select, sub) =
            Self::make_project_select(&self.backend, self.selected_project.as_deref(), window, cx);
        self.project_select = select;
        self.project_sub = sub;
        if self.selected_project != prev {
            self.refresh_skills();
            self.show_chat(None, window, cx);
            self.refresh_sessions(cx);
        }
    }

    fn next_id(&mut self) -> usize {
        self.next_block_id += 1;
        self.next_block_id
    }

    /// Detects user scroll-up vs returning to the bottom without being fooled by content growth.
    ///
    /// In GPUI, `offset.y` is `0` at the top and `-max_offset.y` at the bottom. Content
    /// growing at the bottom never increases `offset.y`; only the user scrolling up does.
    /// Checking upward movement first ensures even a gentle trackpad scroll-up during rapid
    /// token streaming releases auto-scroll immediately without tug-of-war.
    fn sync_scroll_state(&mut self) {
        let cur_y = self.scroll_handle.offset().y;
        let max_y = self.scroll_handle.max_offset().y;
        let dist_from_bottom = max_y + cur_y;
        if max_y <= px(0.0) {
            self.auto_scroll = true;
        } else if cur_y > self.last_offset_y + px(1.0) && dist_from_bottom > px(2.0) {
            self.auto_scroll = false;
        } else if dist_from_bottom <= px(16.0) && cur_y <= self.last_offset_y {
            self.auto_scroll = true;
        }
        self.last_offset_y = cur_y;
    }

    /// Scrolls to the bottom only if the user hasn't scrolled up to read earlier messages.
    fn scroll_if_following(&mut self) {
        self.sync_scroll_state();
        if self.auto_scroll {
            self.scroll_handle.scroll_to_bottom();
        }
    }

    /// Explicitly re-engages bottom-following (e.g. opening a chat, sending, or clicking Jump).
    fn jump_to_bottom(&mut self) {
        self.auto_scroll = true;
        self.scroll_handle.scroll_to_bottom();
        self.last_offset_y = self.scroll_handle.offset().y;
    }

    pub fn select_project(
        &mut self,
        project_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if project_id.is_empty() {
            return;
        }
        self.selected_project = Some(project_id);
        let (select, sub) =
            Self::make_project_select(&self.backend, self.selected_project.as_deref(), window, cx);
        self.project_select = select;
        self.project_sub = sub;
        self.refresh_skills();
        self.show_chat(None, window, cx);
        self.refresh_sessions(cx);
    }

    fn refresh_sessions(&mut self, cx: &mut Context<Self>) {
        let Some(pid) = self.selected_project.clone() else {
            self.sessions.clear();
            self.archived.clear();
            self.starting_note = None;
            self.sessions_error = None;
            cx.notify();
            return;
        };
        self.list_gen += 1;
        let mine = self.list_gen;
        let project = self.backend.app.project(&pid);
        if project.as_ref().is_some_and(|p| p.isolation == Isolation::LinuxVm)
            && self.sessions.is_empty()
        {
            self.starting_note = Some("Starting the Linux VM…".into());
            cx.notify();
        }

        let app = self.backend.app.clone();
        let engine = self.engine;
        let pid_task = pid.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.backend.tokio.spawn(async move {
            let res: Result<(Vec<SessionMeta>, HashSet<String>), String> = async {
                let archived: HashSet<String> = app.archived_chats(&pid_task).into_iter().collect();
                let list = match engine {
                    ChatEngine::OpenCode => {
                        let proj = app
                            .project(&pid_task)
                            .ok_or_else(|| "that project no longer exists".to_string())?;
                        let eng = app
                            .engines
                            .get(&app, &proj)
                            .await
                            .map_err(|e| format!("{e:#}"))?;
                        let raw = eng.sessions().await.map_err(|e| format!("{e:#}"))?;
                        raw.as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|s| {
                                let id = s["sessionId"].as_str()?.to_string();
                                let raw_title = s["title"].as_str().unwrap_or("");
                                let title = if raw_title.starts_with("New session - ")
                                    || raw_title.is_empty()
                                {
                                    "New chat".to_string()
                                } else {
                                    raw_title.to_string()
                                };
                                Some(SessionMeta { id, title })
                            })
                            .collect()
                    }
                    ChatEngine::Builtin => Chat::list(&app.paths, &pid_task)
                        .into_iter()
                        .map(|c| SessionMeta {
                            id: c.id,
                            title: c.title,
                        })
                        .collect(),
                };
                Ok((list, archived))
            }
            .await;
            let _ = tx.send(res);
        });

        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                if this.list_gen != mine || this.selected_project.as_deref() != Some(pid.as_str()) {
                    return;
                }
                this.starting_note = None;
                match result {
                    Ok((sessions, archived)) => {
                        this.sessions = sessions;
                        this.archived = archived;
                        this.sessions_error = None;
                    }
                    Err(e) => {
                        this.sessions_error = Some(e);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn show_chat(&mut self, session_id: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.chat_gen += 1;
        let mine = self.chat_gen;
        self.send_gen += 1;
        self.active_session = session_id.clone();
        self.blocks.clear();
        self.usage = None;
        self.busy = false;
        self.working_since = None;
        self.working_phase = WorkingPhase::ReadingPrompt;
        self.pending_send = None;
        self.confirm_delete_chat = None;
        if !self.creating_session {
            self.buffered_events.clear();
        }

        // Immediately restore known busy state for this session before async load returns.
        if let Some(sid) = session_id.as_deref()
            && let Some(since) = self.working_since_by_session.get(sid).copied()
        {
            self.busy = true;
            self.working_since = Some(since);
            self.working_phase = self
                .working_phase_by_session
                .get(sid)
                .copied()
                .unwrap_or(WorkingPhase::ReadingPrompt);
        }

        // Restore the chat's remembered model if still installed.
        if let (Some(pid), Some(sid)) = (&self.selected_project, &session_id)
            && let Some(own) = self.backend.app.chat_model(pid, sid)
            && self
                .backend
                .app
                .installed()
                .iter()
                .any(|m| m.id == own && self.backend.app.fit(&m.model).is_some())
        {
            self.selected_model = Some(own);
            self.rebuild_model_select(window, cx);
        }

        let (Some(pid), Some(sid)) = (self.selected_project.clone(), session_id) else {
            cx.notify();
            return;
        };

        let app = self.backend.app.clone();
        let engine = self.engine;
        let sid_for_ui = sid.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.backend.tokio.spawn(async move {
            let res: Result<(Option<Instant>, Option<Vec<Value>>), String> = match engine {
                ChatEngine::OpenCode => async {
                    let proj = app
                        .project(&pid)
                        .ok_or_else(|| "that project no longer exists".to_string())?;
                    let eng = app
                        .engines
                        .get(&app, &proj)
                        .await
                        .map_err(|e| format!("{e:#}"))?;
                    if !eng.is_busy(&sid) {
                        eng.load_session(&sid)
                            .await
                            .map_err(|e| format!("{e:#}"))?;
                    }
                    Ok((eng.busy_since(&sid), None))
                }
                .await,
                ChatEngine::Builtin => match Chat::load(&app.paths, &pid, &sid) {
                    Ok(chat) => Ok((app.chats.busy_since(&chat.id), Some(chat.messages))),
                    Err(e) => Err(format!("{e:#}")),
                },
            };
            let _ = tx.send(res);
        });

        cx.notify();
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                if this.chat_gen != mine {
                    return;
                }
                match result {
                    Ok((busy_since, maybe_messages)) => {
                        if let Some(since) = busy_since {
                            let remembered = this
                                .working_since_by_session
                                .entry(sid_for_ui.clone())
                                .or_insert(since);
                            if since < *remembered {
                                *remembered = since;
                            }
                            this.busy = true;
                            this.working_since = Some(*remembered);
                            this.working_phase = this
                                .working_phase_by_session
                                .get(&sid_for_ui)
                                .copied()
                                .unwrap_or(WorkingPhase::ReadingPrompt);
                        } else {
                            this.working_since_by_session.remove(&sid_for_ui);
                            this.working_phase_by_session.remove(&sid_for_ui);
                            this.busy = false;
                            this.working_since = None;
                        }
                        if let Some(messages) = maybe_messages {
                            for (i, msg) in messages.iter().enumerate() {
                                this.apply_builtin_message(msg, Some(format!("m{i}")), true);
                            }
                        }
                        this.jump_to_bottom();
                    }
                    Err(err) => {
                        this.blocks.push(ChatBlock::StatusNote {
                            text: err,
                            is_error: true,
                        });
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn archive_chat(&mut self, chat_id: String, archive: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pid) = self.selected_project.clone() else {
            return;
        };
        if let Err(e) = self.backend.app.set_chat_archived(&pid, &chat_id, archive) {
            self.blocks.push(ChatBlock::StatusNote {
                text: format!("{e:#}"),
                is_error: true,
            });
            cx.notify();
            return;
        }
        if archive {
            self.archived.insert(chat_id.clone());
            if self.active_session.as_deref() == Some(chat_id.as_str()) {
                self.show_chat(None, window, cx);
            }
        } else {
            self.archived.remove(&chat_id);
        }
        cx.notify();
    }

    fn delete_chat(&mut self, chat_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pid) = self.selected_project.clone() else {
            return;
        };
        self.confirm_delete_chat = None;
        let was_active = self.active_session.as_deref() == Some(chat_id.as_str());
        if was_active {
            self.show_chat(None, window, cx);
        }
        let app = self.backend.app.clone();
        let engine = self.engine;
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.backend.tokio.spawn(async move {
            let res: Result<(), String> = match engine {
                ChatEngine::OpenCode => async {
                    let proj = app
                        .project(&pid)
                        .ok_or_else(|| "that project no longer exists".to_string())?;
                    let eng = app
                        .engines
                        .get(&app, &proj)
                        .await
                        .map_err(|e| format!("{e:#}"))?;
                    eng.delete_session(&chat_id)
                        .await
                        .map_err(|e| format!("{e:#}"))?;
                    app.forget_chat(&pid, &chat_id)
                        .map_err(|e| format!("{e:#}"))
                }
                .await,
                ChatEngine::Builtin => {
                    app.chats.stop_and_delete(&chat_id);
                    Chat::delete(&app.paths, &pid, &chat_id)
                        .and_then(|_| app.forget_chat(&pid, &chat_id))
                        .map_err(|e| format!("{e:#}"))
                }
            };
            let _ = tx.send(res);
        });
        cx.spawn(async move |this, cx| {
            let _ = rx.await;
            let _ = this.update(cx, |this, cx| {
                this.refresh_sessions(cx);
            });
        })
        .detach();
    }

    fn send_current(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let raw = self.composer.read(cx).value().to_string();
        let text = raw.trim().to_string();

        // Handle local slash commands even while busy (opening skills panel / creator).
        if let Some(after_slash) = text.strip_prefix('/') {
            let (cmd_raw, rest) = match after_slash.split_once(char::is_whitespace) {
                Some((c, r)) => (c.trim(), r.trim()),
                None => (after_slash.trim(), ""),
            };
            let cmd = cmd_raw.to_ascii_lowercase();
            if matches!(cmd.as_str(), "new-skill" | "create-skill" | "skill-create") {
                self.composer
                    .update(cx, |state, cx| state.set_value("", window, cx));
                self.show_slash_menu = false;
                let (name_hint, goal_hint) = match rest.split_once(char::is_whitespace) {
                    Some((n, g)) => (n.trim(), g.trim()),
                    None => (rest, ""),
                };
                self.open_skill_editor(None, name_hint, goal_hint, window, cx);
                return;
            }
            if cmd == "skills" || cmd.is_empty() {
                self.composer
                    .update(cx, |state, cx| state.set_value("", window, cx));
                self.show_slash_menu = false;
                self.refresh_skills();
                self.show_skills_panel = !self.show_skills_panel || !cmd.is_empty();
                self.skill_editor = None;
                cx.notify();
                return;
            }
        }

        if self.busy {
            return;
        }

        // Resolve slash-activated or pill-activated skill.
        let (display_text, prompt_text) = if let Some(after_slash) = text.strip_prefix('/') {
            let (cmd_raw, rest) = match after_slash.split_once(char::is_whitespace) {
                Some((c, r)) => (c.trim(), r.trim()),
                None => (after_slash.trim(), ""),
            };
            let slug = skills::slugify(cmd_raw);
            let matched = self
                .skills
                .iter()
                .find(|s| s.name == slug)
                .cloned()
                .or_else(|| {
                    let prefix_matches: Vec<&Skill> = self
                        .skills
                        .iter()
                        .filter(|s| s.name.starts_with(&slug))
                        .collect();
                    (prefix_matches.len() == 1).then(|| prefix_matches[0].clone())
                });
            let Some(skill) = matched else {
                self.blocks.push(ChatBlock::StatusNote {
                    text: format!(
                        "Unknown command `/{cmd_raw}`. Type `/skills` to browse skills or `/new-skill` to create one."
                    ),
                    is_error: false,
                });
                cx.notify();
                return;
            };
            let display = if rest.is_empty() {
                format!("/{}", skill.name)
            } else {
                format!("/{} {rest}", skill.name)
            };
            let prompt = skills::format_skill_prompt(&skill, rest);
            (display, prompt)
        } else if let Some(skill) = self.active_skill.clone() {
            let display = if text.is_empty() {
                format!("/{}", skill.name)
            } else {
                format!("/{} {text}", skill.name)
            };
            let prompt = skills::format_skill_prompt(&skill, &text);
            (display, prompt)
        } else {
            if text.is_empty() {
                return;
            }
            (text.clone(), text)
        };

        let (Some(_pid), Some(model_id)) =
            (self.selected_project.clone(), self.selected_model.clone())
        else {
            return;
        };

        // Check if model needs room before clearing composer / sending.
        let loaded = self.backend.app.models();
        let is_running = loaded
            .iter()
            .any(|m| m.id == model_id && matches!(m.state, LoadState::Running { .. }));
        let is_loading = loaded
            .iter()
            .any(|m| m.id == model_id && matches!(m.state, LoadState::Loading { .. }));

        if !is_running && !is_loading {
            match self.backend.app.room_for(&model_id) {
                Ok(room) if !room.fits => {
                    let model_name = self.model_name(&model_id);
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
                    self.pending_send = Some(PendingSend {
                        display_text,
                        prompt_text,
                        model_id,
                        model_name,
                        unload_ids: room.unload,
                        unload_names,
                    });
                    cx.notify();
                    return;
                }
                Err(e) => {
                    self.blocks.push(ChatBlock::StatusNote {
                        text: format!("{e:#}"),
                        is_error: true,
                    });
                    cx.notify();
                    return;
                }
                Ok(_) => {}
            }
        }

        self.active_skill = None;
        self.show_slash_menu = false;
        self.composer
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.dispatch_send(display_text, prompt_text, model_id, Vec::new(), window, cx);
    }

    fn dispatch_send(
        &mut self,
        display_text: String,
        prompt_text: String,
        model_id: String,
        unload_first: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pid) = self.selected_project.clone() else {
            return;
        };
        self.pending_send = None;
        self.active_skill = None;
        self.send_gen += 1;
        let my_send = self.send_gen;
        let my_chat = self.chat_gen;
        let start_session = self.active_session.clone();

        // Optimistic user bubble (for OpenCode, ACP also replays user_message_chunk on load,
        // while during prompt OpenCode doesn't echo our user text until replay).
        let user_block_idx = self.blocks.len();
        self.blocks.push(ChatBlock::User {
            key: None,
            text: display_text.clone(),
        });
        let started_at = Instant::now();
        self.busy = true;
        self.working_since = Some(started_at);
        self.working_phase = WorkingPhase::StartingAgent;
        if let Some(sid) = &start_session {
            self.working_since_by_session.insert(sid.clone(), started_at);
            self.working_phase_by_session
                .insert(sid.clone(), WorkingPhase::StartingAgent);
        }
        if start_session.is_none() {
            self.creating_session = true;
        }
        self.jump_to_bottom();
        cx.notify();

        let app = self.backend.app.clone();
        let engine = self.engine;
        let pid_task = pid.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.backend.tokio.spawn(async move {
            let res: Result<String, String> = async {
                // 1. Wait if already loading, or load (unloading LRU models if confirmed).
                let mut waited = false;
                while app
                    .models()
                    .iter()
                    .any(|m| m.id == model_id && matches!(m.state, LoadState::Loading { .. }))
                {
                    waited = true;
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                let is_loaded = app.loaded(&model_id).is_some();
                if waited && !is_loaded {
                    let err = app
                        .models()
                        .into_iter()
                        .find_map(|m| match m.state {
                            LoadState::Failed { error } if m.id == model_id => Some(error),
                            _ => None,
                        })
                        .unwrap_or_else(|| "the model failed to load".into());
                    return Err(format!("Not sent: {err}"));
                }
                if !is_loaded {
                    for old in unload_first {
                        app.unload_model(Some(&old)).await;
                    }
                    app.load_model(&model_id)
                        .await
                        .map_err(|e| format!("{e:#}"))?;
                }

                // 2. Unarchive if sending to an archived chat.
                if let Some(sid) = &start_session {
                    let _ = app.set_chat_archived(&pid_task, sid, false);
                }

                // 3. Dispatch to OpenCode ACP or Built-in agent.
                let target_session = match engine {
                    ChatEngine::OpenCode => {
                        let (installed_model, _) = app
                            .loaded(&model_id)
                            .ok_or_else(|| "Load the chat's model first.".to_string())?;
                        app.touch(&installed_model.id);
                        app.sync_upstreams().await;
                        let proj = app
                            .project(&pid_task)
                            .ok_or_else(|| "that project no longer exists".to_string())?;
                        let eng = app
                            .engines
                            .get(&app, &proj)
                            .await
                            .map_err(|e| format!("{e:#}"))?;
                        let sid = match start_session {
                            Some(s) => s,
                            None => eng.new_session().await.map_err(|e| format!("{e:#}"))?,
                        };
                        eng.prompt(app.bus.clone(), &sid, &prompt_text, &installed_model.alias)
                            .map_err(|e| format!("{e:#}"))?;
                        sid
                    }
                    ChatEngine::Builtin => superviser_backend::agent::send(
                        app.clone(),
                        &pid_task,
                        start_session.as_deref(),
                        &prompt_text,
                        &model_id,
                    )
                    .map_err(|e| format!("{e:#}"))?,
                };
                let _ = app.set_chat_model(&pid_task, &target_session, Some(model_id));
                Ok(target_session)
            }
            .await;
            let _ = tx.send(res);
        });

        cx.spawn_in(window, async move |this, cx| {
            let Ok(result) = rx.await else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.creating_session = false;
                if this.chat_gen != my_chat {
                    if let Ok(target_session) = result {
                        this.working_since_by_session
                            .entry(target_session.clone())
                            .or_insert(started_at);
                        this.working_phase_by_session
                            .entry(target_session)
                            .or_insert(WorkingPhase::ReadingPrompt);
                    }
                    this.buffered_events.clear();
                    return;
                }
                let cancelled = this.send_gen != my_send;
                match result {
                    Ok(target_session) => {
                        if cancelled {
                            this.cancel_remote(&pid, &target_session);
                            this.buffered_events.clear();
                            return;
                        }
                        this.archived.remove(&target_session);
                        this.active_session = Some(target_session.clone());
                        this.working_since_by_session
                            .entry(target_session.clone())
                            .or_insert(started_at);
                        if this.working_phase == WorkingPhase::StartingAgent {
                            this.working_phase = WorkingPhase::ReadingPrompt;
                        }
                        this.working_phase_by_session
                            .insert(target_session, this.working_phase);
                        let buffered = std::mem::take(&mut this.buffered_events);
                        for ev in buffered {
                            this.handle_app_event(ev, window, cx);
                        }
                        this.refresh_sessions(cx);
                        cx.notify();
                    }
                    Err(err) => {
                        this.buffered_events.clear();
                        // Remove optimistic user bubble and restore unsent text.
                        if user_block_idx < this.blocks.len() {
                            this.blocks.remove(user_block_idx);
                        }
                        if this.composer.read(cx).value().trim().is_empty() {
                            this.composer.update(cx, |s, cx| {
                                s.set_value(display_text.clone(), window, cx)
                            });
                        }
                        if let Some(sid) = &this.active_session {
                            this.working_since_by_session.remove(sid);
                            this.working_phase_by_session.remove(sid);
                        }
                        this.busy = false;
                        this.working_since = None;
                        if !cancelled {
                            this.blocks.push(ChatBlock::StatusNote {
                                text: err,
                                is_error: true,
                            });
                        }
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn stop_current(&mut self, cx: &mut Context<Self>) {
        self.send_gen += 1;
        if let (Some(pid), Some(sid)) = (self.selected_project.clone(), self.active_session.clone()) {
            self.cancel_remote(&pid, &sid);
        } else {
            // Cancelled before session existed (e.g. while loading model).
            self.busy = false;
            self.working_since = None;
            self.creating_session = false;
            self.blocks.push(ChatBlock::StatusNote {
                text: "Stopped.".into(),
                is_error: false,
            });
            cx.notify();
        }
    }

    fn cancel_remote(&self, project_id: &str, session_id: &str) {
        let app = self.backend.app.clone();
        let pid = project_id.to_string();
        let sid = session_id.to_string();
        match self.engine {
            ChatEngine::OpenCode => {
                self.backend.tokio.spawn(async move {
                    if let Some(eng) = app.engines.running(&pid).await {
                        let _ = eng.cancel(&sid).await;
                    }
                });
            }
            ChatEngine::Builtin => {
                app.chats.stop(&sid);
            }
        }
    }

    fn answer_permission(
        &mut self,
        block_id: usize,
        request_id: String,
        option: PermissionOption,
        cx: &mut Context<Self>,
    ) {
        let allow = option.kind.starts_with("allow");
        for block in &mut self.blocks {
            if let ChatBlock::ToolCall(tc) = block
                && tc.id == block_id
            {
                tc.approval = None;
                tc.status = if allow {
                    "running…".into()
                } else {
                    "declined".into()
                };
            }
        }
        cx.notify();

        let app = self.backend.app.clone();
        match self.engine {
            ChatEngine::OpenCode => {
                let Some(pid) = self.selected_project.clone() else {
                    return;
                };
                let opt_id = option.option_id;
                self.backend.tokio.spawn(async move {
                    if let Some(eng) = app.engines.running(&pid).await {
                        let _ = eng.answer(&request_id, Some(&opt_id)).await;
                    }
                });
            }
            ChatEngine::Builtin => {
                if let Some(sid) = self.active_session.clone() {
                    app.chats.answer(&sid, &request_id, allow);
                }
            }
        }
    }

    fn handle_app_event(
        &mut self,
        event: AppEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.creating_session && matches!(event, AppEvent::Acp(_) | AppEvent::Chat(_)) {
            self.buffered_events.push(event);
            return;
        }

        match event {
            AppEvent::Status(_) | AppEvent::Discovery | AppEvent::Judge => {
                self.rebuild_model_select(window, cx);
                cx.notify();
            }
            AppEvent::LoadProgress { .. } => {
                cx.notify();
            }
            AppEvent::Reasoning => {
                self.rebuild_reasoning_select(window, cx);
                cx.notify();
            }
            AppEvent::Projects => {
                self.rebuild_project_select(window, cx);
                cx.notify();
            }
            AppEvent::Acp(ev) => self.handle_acp_event(ev, cx),
            AppEvent::Chat(ev) => self.handle_chat_event(ev, cx),
            AppEvent::Download(dl) if dl.finished => {
                self.rebuild_model_select(window, cx);
                cx.notify();
            }
            AppEvent::GuardCall(_) | AppEvent::Download(_) => {}
        }
    }

    fn handle_acp_event(&mut self, event: AcpEvent, cx: &mut Context<Self>) {
        match event {
            AcpEvent::Starting { project, text } => {
                if self.selected_project.as_deref() == Some(project.as_str()) {
                    self.starting_note = Some(text);
                    cx.notify();
                }
            }
            AcpEvent::Update {
                project,
                session,
                update,
            } => {
                if let Some(phase) = acp_update_phase(&update) {
                    self.working_phase_by_session.insert(session.clone(), phase);
                    if self.active_session.as_deref() == Some(session.as_str()) {
                        self.working_phase = phase;
                    }
                }
                if self.selected_project.as_deref() != Some(project.as_str())
                    || self.active_session.as_deref() != Some(session.as_str())
                {
                    return;
                }
                self.apply_acp_update(&update);
                self.scroll_if_following();
                cx.notify();
            }
            AcpEvent::Permission {
                project,
                session,
                request,
                tool_call,
                options,
            } => {
                if self.selected_project.as_deref() != Some(project.as_str())
                    || self.active_session.as_deref() != Some(session.as_str())
                {
                    return;
                }
                self.upsert_tool_call(&tool_call);
                let call_id = tool_call["toolCallId"].as_str().unwrap_or("").to_string();
                let parsed_opts = parse_permission_options(&options);
                for block in &mut self.blocks {
                    if let ChatBlock::ToolCall(tc) = block
                        && tc.call_id == call_id
                    {
                        tc.open = true;
                        tc.status = "waiting for you".into();
                        tc.approval = Some(PendingApproval {
                            request_id: request.clone(),
                            options: parsed_opts.clone(),
                        });
                    }
                }
                self.scroll_if_following();
                cx.notify();
            }
            AcpEvent::State {
                project,
                session,
                busy,
                stop_reason,
                error,
            } => {
                if busy {
                    let since = *self
                        .working_since_by_session
                        .entry(session.clone())
                        .or_insert_with(Instant::now);
                    let phase = *self
                        .working_phase_by_session
                        .entry(session.clone())
                        .or_insert(WorkingPhase::ReadingPrompt);
                    if self.selected_project.as_deref() == Some(project.as_str())
                        && self.active_session.as_deref() == Some(session.as_str())
                    {
                        self.busy = true;
                        if self.working_since.is_none() {
                            self.working_since = Some(since);
                        }
                        self.working_phase = phase;
                        cx.notify();
                    }
                } else {
                    self.working_since_by_session.remove(&session);
                    self.working_phase_by_session.remove(&session);
                    if self.selected_project.as_deref() != Some(project.as_str()) {
                        return;
                    }
                    if self.active_session.as_deref() != Some(session.as_str()) {
                        self.refresh_sessions(cx);
                        return;
                    }
                    self.finish_turn(error, stop_reason, cx);
                    cx.notify();
                }
            }
        }
    }

    fn handle_chat_event(&mut self, event: ChatEvent, cx: &mut Context<Self>) {
        match event {
            ChatEvent::Message { chat, message } => {
                if message["role"].as_str() == Some("assistant")
                    && message["tool_calls"].as_array().is_some_and(|a| !a.is_empty())
                {
                    self.working_phase_by_session
                        .insert(chat.clone(), WorkingPhase::RunningTool);
                    if self.active_session.as_deref() == Some(chat.as_str()) {
                        self.working_phase = WorkingPhase::RunningTool;
                    }
                } else if message["role"].as_str() == Some("tool") {
                    self.working_phase_by_session
                        .insert(chat.clone(), WorkingPhase::ReadingPrompt);
                    if self.active_session.as_deref() == Some(chat.as_str()) {
                        self.working_phase = WorkingPhase::ReadingPrompt;
                    }
                }
                if self.active_session.as_deref() != Some(chat.as_str()) {
                    return;
                }
                self.apply_builtin_message(&message, None, false);
                self.scroll_if_following();
                cx.notify();
            }
            ChatEvent::Delta {
                chat,
                content,
                reasoning,
            } => {
                if !content.is_empty() {
                    self.working_phase_by_session
                        .insert(chat.clone(), WorkingPhase::Writing);
                    if self.active_session.as_deref() == Some(chat.as_str()) {
                        self.working_phase = WorkingPhase::Writing;
                    }
                } else if !reasoning.is_empty() {
                    self.working_phase_by_session
                        .insert(chat.clone(), WorkingPhase::Thinking);
                    if self.active_session.as_deref() == Some(chat.as_str()) {
                        self.working_phase = WorkingPhase::Thinking;
                    }
                }
                if self.active_session.as_deref() != Some(chat.as_str()) {
                    return;
                }
                if !reasoning.is_empty() {
                    self.upsert_thought("live-thought", &reasoning, true);
                }
                if !content.is_empty() {
                    self.upsert_assistant("live-text", &content, true);
                }
                self.scroll_if_following();
                cx.notify();
            }
            ChatEvent::Approval { chat, call, .. } => {
                if self.active_session.as_deref() != Some(chat.as_str()) {
                    return;
                }
                for block in &mut self.blocks {
                    if let ChatBlock::ToolCall(tc) = block
                        && tc.call_id == call
                    {
                        tc.open = true;
                        tc.status = "waiting for you".into();
                        tc.approval = Some(PendingApproval {
                            request_id: call.clone(),
                            options: vec![
                                PermissionOption {
                                    option_id: "allow".into(),
                                    kind: "allow_once".into(),
                                    label: "Allow".into(),
                                },
                                PermissionOption {
                                    option_id: "deny".into(),
                                    kind: "reject_once".into(),
                                    label: "Deny".into(),
                                },
                            ],
                        });
                    }
                }
                self.scroll_if_following();
                cx.notify();
            }
            ChatEvent::State { chat, busy, error } => {
                if busy {
                    let since = *self
                        .working_since_by_session
                        .entry(chat.clone())
                        .or_insert_with(Instant::now);
                    let phase = *self
                        .working_phase_by_session
                        .entry(chat.clone())
                        .or_insert(WorkingPhase::ReadingPrompt);
                    if self.active_session.as_deref() == Some(chat.as_str()) {
                        self.busy = true;
                        if self.working_since.is_none() {
                            self.working_since = Some(since);
                        }
                        self.working_phase = phase;
                        cx.notify();
                    }
                } else {
                    self.working_since_by_session.remove(&chat);
                    self.working_phase_by_session.remove(&chat);
                    if self.active_session.as_deref() != Some(chat.as_str()) {
                        self.refresh_sessions(cx);
                        return;
                    }
                    self.finish_turn(error, None, cx);
                    cx.notify();
                }
            }
        }
    }

    fn finish_turn(
        &mut self,
        error: Option<String>,
        stop_reason: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(sid) = &self.active_session {
            self.working_since_by_session.remove(sid);
            self.working_phase_by_session.remove(sid);
        }
        self.busy = false;
        self.working_since = None;
        self.working_phase = WorkingPhase::ReadingPrompt;
        self.seal_live_blocks();
        if stop_reason.as_deref() == Some("cancelled") {
            self.blocks.push(ChatBlock::StatusNote {
                text: "Stopped.".into(),
                is_error: false,
            });
        }
        if let Some(err) = error {
            self.blocks.push(ChatBlock::StatusNote {
                text: err,
                is_error: true,
            });
        }
        self.refresh_sessions(cx);
        self.scroll_if_following();
    }

    fn seal_live_blocks(&mut self) {
        for block in &mut self.blocks {
            match block {
                ChatBlock::Assistant { key, .. } if key == "current" || key == "live-text" => {
                    *key = format!("sealed-{}", self.next_block_id);
                    self.next_block_id += 1;
                }
                ChatBlock::Thinking { key, .. } if key == "current" || key == "live-thought" => {
                    *key = format!("sealed-{}", self.next_block_id);
                    self.next_block_id += 1;
                }
                _ => {}
            }
        }
    }

    fn apply_acp_update(&mut self, update: &Value) {
        let kind = update["sessionUpdate"].as_str().unwrap_or("");
        match kind {
            "user_message_chunk" => {
                let text = content_text(&update["content"]);
                if text.is_empty() {
                    return;
                }
                self.seal_live_blocks();
                let msg_id = update["messageId"].as_str().map(str::to_string);
                if let Some(ref mid) = msg_id
                    && let Some(ChatBlock::User { text: existing, .. }) = self.blocks.iter_mut().find(
                        |b| matches!(b, ChatBlock::User { key: Some(k), .. } if k == mid),
                    )
                {
                    existing.push_str(&text);
                    return;
                }
                self.blocks.push(ChatBlock::User { key: msg_id, text });
            }
            "agent_message_chunk" => {
                let text = content_text(&update["content"]);
                let key = update["messageId"].as_str().unwrap_or("current");
                self.upsert_assistant(key, &text, false);
            }
            "agent_thought_chunk" => {
                let text = content_text(&update["content"]);
                let key = update["messageId"].as_str().unwrap_or("current");
                self.upsert_thought(key, &text, false);
            }
            "tool_call" | "tool_call_update" => {
                self.upsert_tool_call(update);
            }
            "usage_update" => {
                let used = update["used"].as_u64().unwrap_or(0);
                let size = update["size"].as_u64().unwrap_or(0);
                if size > 0 {
                    self.usage = Some((used, size));
                }
            }
            _ => {}
        }
    }

    fn apply_builtin_message(&mut self, message: &Value, replay_id: Option<String>, replay: bool) {
        let role = message["role"].as_str().unwrap_or("");
        match role {
            "user" => {
                if replay {
                    let text = message["content"].as_str().unwrap_or("").to_string();
                    self.blocks.push(ChatBlock::User {
                        key: replay_id,
                        text,
                    });
                }
            }
            "assistant" => {
                if let Some(reasoning) = message["reasoning"].as_str()
                    && !reasoning.is_empty()
                {
                    let key = replay_id
                        .as_deref()
                        .unwrap_or("live-thought");
                    self.upsert_thought(key, reasoning, true);
                }
                let content = message["content"].as_str().unwrap_or("");
                if !content.is_empty() {
                    let key = replay_id.as_deref().unwrap_or("live-text");
                    self.upsert_assistant(key, content, true);
                }
                if !replay {
                    self.seal_live_blocks();
                }
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    let cid = call["id"].as_str().unwrap_or("").to_string();
                    let name = call["function"]["name"].as_str().unwrap_or("tool");
                    let args_str = call["function"]["arguments"].as_str().unwrap_or("{}");
                    let raw_input: Value =
                        serde_json::from_str(args_str).unwrap_or_else(|_| Value::Object(Default::default()));
                    let kind = match name {
                        "read" => "read",
                        "glob" | "grep" => "search",
                        "write" | "edit" => "edit",
                        "bash" => "execute",
                        _ => "other",
                    };
                    let synth = serde_json::json!({
                        "toolCallId": cid,
                        "name": name,
                        "title": name,
                        "kind": kind,
                        "status": "in_progress",
                        "rawInput": raw_input,
                    });
                    self.upsert_tool_call(&synth);
                }
            }
            "tool" => {
                let cid = message["tool_call_id"].as_str().unwrap_or("");
                let text = message["content"].as_str().unwrap_or("");
                let failed = text.starts_with("Error:")
                    || text.starts_with("The user declined")
                    || text.starts_with("Not run:");
                let synth = serde_json::json!({
                    "toolCallId": cid,
                    "status": if failed { "failed" } else { "completed" },
                    "content": [{ "type": "content", "content": { "type": "text", "text": text } }],
                });
                self.upsert_tool_call(&synth);
            }
            _ => {}
        }
    }

    fn upsert_assistant(&mut self, key: &str, chunk: &str, replace: bool) {
        if let Some(ChatBlock::Assistant {
            raw_text,
            text,
            guard_notes,
            ..
        }) = self
            .blocks
            .iter_mut()
            .rev()
            .find(|b| matches!(b, ChatBlock::Assistant { key: k, .. } if k == key))
        {
            if replace {
                *raw_text = chunk.to_string();
            } else {
                raw_text.push_str(chunk);
            }
            let (clean, notes) = split_guard_notes(raw_text);
            *text = clean;
            *guard_notes = notes;
            return;
        }
        let (clean, notes) = split_guard_notes(chunk);
        if clean.trim().is_empty() && notes.is_empty() {
            return;
        }
        let id = self.next_id();
        self.blocks.push(ChatBlock::Assistant {
            id,
            key: key.to_string(),
            raw_text: chunk.to_string(),
            text: clean,
            guard_notes: notes,
        });
    }

    fn upsert_thought(&mut self, key: &str, chunk: &str, replace: bool) {
        if let Some(ChatBlock::Thinking { text, .. }) = self
            .blocks
            .iter_mut()
            .rev()
            .find(|b| matches!(b, ChatBlock::Thinking { key: k, .. } if k == key))
        {
            if replace {
                *text = chunk.to_string();
            } else {
                text.push_str(chunk);
            }
            return;
        }
        if chunk.is_empty() {
            return;
        }
        let id = self.next_id();
        self.blocks.push(ChatBlock::Thinking {
            id,
            key: key.to_string(),
            text: chunk.to_string(),
            open: false,
        });
    }

    fn upsert_tool_call(&mut self, update: &Value) {
        let Some(call_id) = update["toolCallId"].as_str() else {
            return;
        };
        let proj = self
            .selected_project
            .as_deref()
            .and_then(|id| self.backend.app.project(id));
        let main_path = proj.as_ref().map(|p| match p.isolation {
            Isolation::LinuxVm => "/workspace".to_string(),
            Isolation::MacosSandbox => p.main().display().to_string(),
        });

        let existing_idx = self.blocks.iter().position(
            |b| matches!(b, ChatBlock::ToolCall(tc) if tc.call_id == call_id),
        );

        let block_idx = if let Some(idx) = existing_idx {
            idx
        } else {
            let id = self.next_id();
            self.blocks.push(ChatBlock::ToolCall(Box::new(ToolCallBlock {
                id,
                call_id: call_id.to_string(),
                kind: String::new(),
                verb: "Tool".into(),
                subject: String::new(),
                status: String::new(),
                open: false,
                diff: Vec::new(),
                output: None,
                approval: None,
                raw: Value::Object(Default::default()),
            })));
            self.blocks.len() - 1
        };

        if let ChatBlock::ToolCall(tc) = &mut self.blocks[block_idx] {
            let ToolCallBlock {
                kind,
                verb,
                subject,
                status,
                open,
                diff,
                output,
                approval,
                raw,
                ..
            } = tc.as_mut();
            // Merge non-null, non-empty fields from partial ACP updates.
            if let (Some(dst), Some(src)) = (raw.as_object_mut(), update.as_object()) {
                for (k, v) in src {
                    if v.is_null() {
                        continue;
                    }
                    if let Some(arr) = v.as_array()
                        && arr.is_empty()
                        && dst.get(k).and_then(Value::as_array).is_some_and(|a| !a.is_empty())
                    {
                        continue;
                    }
                    dst.insert(k.clone(), v.clone());
                }
            }

            let call_kind = raw["kind"].as_str().unwrap_or("");
            *kind = call_kind.to_string();
            *verb = tool_verb(call_kind, raw);
            *subject = tool_subject(call_kind, raw, main_path.as_deref());

            let raw_status = raw["status"].as_str().unwrap_or("");
            if raw_status == "completed" || raw_status == "failed" {
                *approval = None;
            }
            if approval.is_none() {
                *status = match raw_status {
                    "in_progress" => "running…".into(),
                    "failed" => "failed".into(),
                    _ => String::new(),
                };
            }

            let mut new_diff = Vec::new();
            let mut texts = Vec::new();
            for item in raw["content"].as_array().into_iter().flatten() {
                match item["type"].as_str().unwrap_or("") {
                    "diff" => {
                        let old_t = item["oldText"].as_str().unwrap_or("");
                        let new_t = item["newText"].as_str().unwrap_or("");
                        new_diff.extend(compute_diff(old_t, new_t));
                    }
                    "content" => {
                        let t = content_text(&item["content"]);
                        if !t.is_empty() {
                            texts.push(t);
                        }
                    }
                    _ => {}
                }
            }
            if new_diff.is_empty() && call_kind == "edit" {
                let raw_in = &raw["rawInput"];
                let old_s = raw_in["oldString"]
                    .as_str()
                    .or_else(|| raw_in["old_string"].as_str());
                let new_s = raw_in["newString"]
                    .as_str()
                    .or_else(|| raw_in["new_string"].as_str());
                if old_s.is_some() || new_s.is_some() {
                    new_diff = compute_diff(old_s.unwrap_or(""), new_s.unwrap_or(""));
                }
            }
            *diff = new_diff;
            *output = (!texts.is_empty()).then(|| texts.join("\n"));
            if call_kind == "edit" && (!diff.is_empty() || output.is_some()) {
                *open = true;
            }
        }
    }

    fn toggle_block_open(&mut self, block_id: usize, cx: &mut Context<Self>) {
        for block in &mut self.blocks {
            match block {
                ChatBlock::Thinking { id, open, .. } if *id == block_id => *open = !*open,
                ChatBlock::ToolCall(tc) if tc.id == block_id => tc.open = !tc.open,
                _ => {}
            }
        }
        cx.notify();
    }

    fn model_name(&self, id: &str) -> String {
        self.backend
            .app
            .find_installed(id)
            .map(|m| m.model.name)
            .unwrap_or_else(|| id.to_string())
    }

    fn current_project_root(&self) -> Option<std::path::PathBuf> {
        self.selected_project
            .as_deref()
            .and_then(|id| self.backend.app.project(id))
            .map(|p| p.main().to_path_buf())
    }

    fn refresh_skills(&mut self) {
        let root = self.current_project_root();
        self.skills = skills::discover(root.as_deref(), &self.backend.app.paths.data);
        if let Some(active) = &self.active_skill
            && let Some(updated) = self.skills.iter().find(|s| s.name == active.name).cloned()
        {
            self.active_skill = Some(updated);
        } else if self.active_skill.is_some() {
            self.active_skill = None;
        }
    }

    fn open_skill_editor(
        &mut self,
        existing: Option<Skill>,
        name_hint: &str,
        goal_hint: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let scope = existing
            .as_ref()
            .map(|s| s.scope)
            .unwrap_or_else(|| {
                if self.selected_project.is_some() {
                    SkillScope::Project
                } else {
                    SkillScope::Global
                }
            });
        let init_name = existing
            .as_ref()
            .map(|s| s.name.clone())
            .unwrap_or_else(|| skills::slugify(name_hint));
        let init_desc = existing
            .as_ref()
            .map(|s| s.description.clone())
            .unwrap_or_else(|| goal_hint.to_string());
        let init_body = existing
            .as_ref()
            .map(|s| s.instructions.clone())
            .unwrap_or_else(|| {
                if goal_hint.is_empty() {
                    String::new()
                } else {
                    goal_hint.to_string()
                }
            });

        let name_input = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("e.g. code-review");
            s.set_value(init_name, window, cx);
            s
        });
        let desc_input = cx.new(|cx| {
            let mut s = InputState::new(window, cx)
                .placeholder("Short 1-line summary shown in the / menu");
            s.set_value(init_desc, window, cx);
            s
        });
        let body_input = cx.new(|cx| {
            let mut s = TextareaState::new(window, cx)
                .placeholder("Step-by-step instructions for the agent (or click Draft with AI)")
                .rows(4)
                .auto_grow(3, 8);
            s.set_value(init_body, window, cx);
            s
        });

        self.show_skills_panel = false;
        self.show_slash_menu = false;
        self.skill_notice = None;
        self.skill_editor = Some(SkillEditor {
            scope,
            name_input,
            desc_input,
            body_input,
            editing_original: existing,
        });
        cx.notify();
    }

    fn save_skill_editor(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.skill_editor.as_ref() else {
            return;
        };
        let scope = editor.scope;
        let name = editor.name_input.read(cx).value().to_string();
        let desc = editor.desc_input.read(cx).value().to_string();
        let body = editor.body_input.read(cx).value().to_string();
        let prev = editor.editing_original.clone();
        let root = self.current_project_root();

        match skills::save_skill(
            scope,
            root.as_deref(),
            &self.backend.app.paths.data,
            &name,
            &desc,
            &body,
        ) {
            Ok(saved) => {
                if let Some(old) = prev
                    && (old.path != saved.path)
                {
                    let _ = skills::delete_skill(&old);
                }
                self.skill_editor = None;
                self.skill_notice = None;
                self.refresh_skills();
                self.active_skill = Some(saved.clone());
                self.blocks.push(ChatBlock::StatusNote {
                    text: format!(
                        "Saved {} skill `/{}`. Use `/{}` anytime to activate it.",
                        saved.scope.label().to_lowercase(),
                        saved.name,
                        saved.name
                    ),
                    is_error: false,
                });
                cx.notify();
            }
            Err(e) => {
                self.skill_notice = Some((format!("{e:#}"), true));
                cx.notify();
            }
        }
    }

    fn draft_skill_with_ai(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.skill_drafting {
            return;
        }
        let Some(editor) = self.skill_editor.as_ref() else {
            return;
        };
        let name_hint = editor.name_input.read(cx).value().trim().to_string();
        let desc_hint = editor.desc_input.read(cx).value().trim().to_string();
        let body_hint = editor.body_input.read(cx).value().trim().to_string();
        let goal = if !body_hint.is_empty() {
            body_hint
        } else if !desc_hint.is_empty() {
            desc_hint
        } else if !name_hint.is_empty() {
            name_hint.clone()
        } else {
            self.skill_notice = Some((
                "Type a skill name or a short idea first, then click Draft with AI.".into(),
                true,
            ));
            cx.notify();
            return;
        };
        let Some((model_id, _)) = self.loaded_explain_model() else {
            self.skill_notice = Some((
                "Load a model first to draft a skill with AI.".into(),
                true,
            ));
            cx.notify();
            return;
        };

        self.skill_drafting = true;
        self.skill_notice = Some(("Drafting skill instructions with your model…".into(), false));
        cx.notify();

        let app = self.backend.app.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.backend.tokio.spawn(async move {
            let res = skills::draft_skill_with_model(&app, &name_hint, &goal, Some(&model_id))
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(res);
        });

        cx.spawn_in(window, async move |this, cx| {
            let Ok(result) = rx.await else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.skill_drafting = false;
                match result {
                    Ok((slug, desc, body)) => {
                        if let Some(editor) = &this.skill_editor {
                            if editor.name_input.read(cx).value().trim().is_empty() && !slug.is_empty() {
                                editor
                                    .name_input
                                    .update(cx, |s, cx| s.set_value(slug, window, cx));
                            }
                            if !desc.is_empty() {
                                editor
                                    .desc_input
                                    .update(cx, |s, cx| s.set_value(desc, window, cx));
                            }
                            editor
                                .body_input
                                .update(cx, |s, cx| s.set_value(body, window, cx));
                        }
                        this.skill_notice = Some(("Draft ready — review and click Save skill.".into(), false));
                    }
                    Err(err) => {
                        this.skill_notice = Some((err, true));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn delete_skill_entry(&mut self, skill: Skill, cx: &mut Context<Self>) {
        match skills::delete_skill(&skill) {
            Ok(()) => {
                if self
                    .active_skill
                    .as_ref()
                    .is_some_and(|a| a.name == skill.name)
                {
                    self.active_skill = None;
                }
                self.refresh_skills();
                self.skill_notice = None;
            }
            Err(e) => {
                self.skill_notice = Some((format!("{e:#}"), true));
            }
        }
        cx.notify();
    }

    fn render_slash_menu(
        &self,
        query: &str,
        p: crate::theme::Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let q = query.trim_start_matches('/').to_ascii_lowercase();
        let show_new = q.is_empty() || "new-skill".contains(&q) || "create-skill".contains(&q);
        let show_list = q.is_empty() || "skills".contains(&q);
        let matching_skills: Vec<Skill> = self
            .skills
            .iter()
            .filter(|s| {
                q.is_empty()
                    || s.name.contains(&q)
                    || s.description.to_ascii_lowercase().contains(&q)
            })
            .cloned()
            .collect();

        v_flex()
            .id("slash-menu-scroll")
            .flex_shrink_0()
            .mx_4()
            .mb_1()
            .p_2()
            .gap_1()
            .max_h(px(220.0))
            .overflow_y_scroll()
            .rounded(px(8.0))
            .border_1()
            .border_color(p.line)
            .bg(p.panel)
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .px_2()
                    .pb_1()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                            .text_color(p.muted)
                            .child("Slash commands & skills"),
                    )
                    .child(
                        Button::new("slash-menu-close")
                            .ghost()
                            .xsmall()
                            .label("×")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_slash_menu = false;
                                cx.notify();
                            })),
                    ),
            )
            .children(matching_skills.into_iter().enumerate().map(|(i, skill)| {
                let name = skill.name.clone();
                let desc = skill.description.clone();
                let scope_label = skill.scope.label();
                h_flex()
                    .id(("slash-skill", i))
                    .cursor_pointer()
                    .px_2()
                    .py(px(5.0))
                    .gap_2()
                    .items_center()
                    .rounded(px(6.0))
                    .hover(move |d| d.bg(p.chip))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.active_skill = Some(skill.clone());
                        this.show_slash_menu = false;
                        if this.composer.read(cx).value().trim_start().starts_with('/') {
                            this.composer
                                .update(cx, |s, cx| s.set_value("", window, cx));
                        }
                        cx.notify();
                    }))
                    .child(
                        div()
                            .text_xs()
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .text_color(p.accent)
                            .child(format!("/{name}")),
                    )
                    .child(Tag::secondary().small().child(scope_label))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(p.muted)
                            .truncate()
                            .child(desc),
                    )
            }))
            .when(show_new, |col| {
                col.child(
                    h_flex()
                        .id("slash-new-skill")
                        .cursor_pointer()
                        .px_2()
                        .py(px(5.0))
                        .gap_2()
                        .items_center()
                        .rounded(px(6.0))
                        .hover(move |d| d.bg(p.chip))
                        .on_click(cx.listener(|this, _, window, cx| {
                            if this.composer.read(cx).value().trim_start().starts_with('/') {
                                this.composer
                                    .update(cx, |s, cx| s.set_value("", window, cx));
                            }
                            this.open_skill_editor(None, "", "", window, cx);
                        }))
                        .child(
                            div()
                                .text_xs()
                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                .text_color(p.text)
                                .child("/new-skill"),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(p.muted)
                                .child("Create a new reusable project or global skill"),
                        ),
                )
            })
            .when(show_list, |col| {
                col.child(
                    h_flex()
                        .id("slash-list-skills")
                        .cursor_pointer()
                        .px_2()
                        .py(px(5.0))
                        .gap_2()
                        .items_center()
                        .rounded(px(6.0))
                        .hover(move |d| d.bg(p.chip))
                        .on_click(cx.listener(|this, _, window, cx| {
                            if this.composer.read(cx).value().trim_start().starts_with('/') {
                                this.composer
                                    .update(cx, |s, cx| s.set_value("", window, cx));
                            }
                            this.show_slash_menu = false;
                            this.refresh_skills();
                            this.show_skills_panel = true;
                            this.skill_editor = None;
                            cx.notify();
                        }))
                        .child(
                            div()
                                .text_xs()
                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                .text_color(p.text)
                                .child("/skills"),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(p.muted)
                                .child(format!("Browse & manage saved skills ({})", self.skills.len())),
                        ),
                )
            })
    }

    fn render_skills_panel(
        &self,
        p: crate::theme::Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .id("skills-panel-scroll")
            .flex_shrink_0()
            .mx_4()
            .mb_2()
            .p_3()
            .gap_2()
            .max_h(px(260.0))
            .overflow_y_scroll()
            .rounded(px(8.0))
            .border_1()
            .border_color(p.line)
            .bg(p.panel)
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .child("Skills"),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("skills-panel-new")
                                    .primary()
                                    .xsmall()
                                    .label("+ New skill")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_skill_editor(None, "", "", window, cx);
                                    })),
                            )
                            .child(
                                Button::new("skills-panel-close")
                                    .ghost()
                                    .xsmall()
                                    .label("Close")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.show_skills_panel = false;
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .when_some(self.skill_notice.as_ref(), |col, (msg, is_err)| {
                col.child(
                    div()
                        .text_xs()
                        .text_color(if *is_err { p.bad } else { p.muted })
                        .child(msg.clone()),
                )
            })
            .when(self.skills.is_empty(), |col| {
                col.child(div().text_xs().text_color(p.muted).child(
                    "No skills yet. Click “+ New skill” or type /new-skill to save reusable instructions (stored in .opencode/skills/ so OpenCode can use them too).",
                ))
            })
            .children(self.skills.iter().cloned().enumerate().map(|(i, skill)| {
                let s_use = skill.clone();
                let s_edit = skill.clone();
                let s_del = skill.clone();
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .items_center()
                    .rounded(px(6.0))
                    .bg(p.bg)
                    .border_1()
                    .border_color(p.line)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                            .text_color(p.accent)
                                            .child(format!("/{}", skill.name)),
                                    )
                                    .child(Tag::secondary().small().child(skill.scope.label())),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(p.muted)
                                    .truncate()
                                    .child(skill.description.clone()),
                            ),
                    )
                    .child(
                        Button::new(("skill-use", i))
                            .primary()
                            .xsmall()
                            .label("Use")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.active_skill = Some(s_use.clone());
                                this.show_skills_panel = false;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new(("skill-edit", i))
                            .xsmall()
                            .label("Edit")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_skill_editor(Some(s_edit.clone()), "", "", window, cx);
                            })),
                    )
                    .child(
                        Button::new(("skill-del", i))
                            .ghost()
                            .xsmall()
                            .label("×")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.delete_skill_entry(s_del.clone(), cx);
                            })),
                    )
            }))
    }

    fn render_skill_editor(
        &self,
        editor: &SkillEditor,
        p: crate::theme::Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let has_project = self.selected_project.is_some();
        let has_loaded_model = self.loaded_explain_model().is_some();
        let title = if editor.editing_original.is_some() {
            "Edit skill"
        } else {
            "New skill"
        };

        v_flex()
            .flex_shrink_0()
            .mx_4()
            .mb_2()
            .p_3()
            .gap_2()
            .rounded(px(8.0))
            .border_1()
            .border_color(p.accent.opacity(0.6))
            .bg(p.panel)
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(
                                Button::new("skill-scope-project")
                                    .when(editor.scope == SkillScope::Project, |b| b.primary())
                                    .xsmall()
                                    .disabled(!has_project)
                                    .label("Project")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(ed) = &mut this.skill_editor {
                                            ed.scope = SkillScope::Project;
                                            cx.notify();
                                        }
                                    })),
                            )
                            .child(
                                Button::new("skill-scope-global")
                                    .when(editor.scope == SkillScope::Global, |b| b.primary())
                                    .xsmall()
                                    .label("Global")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(ed) = &mut this.skill_editor {
                                            ed.scope = SkillScope::Global;
                                            cx.notify();
                                        }
                                    })),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        v_flex()
                            .w(px(190.0))
                            .gap_1()
                            .child(div().text_xs().text_color(p.muted).child("Slash name"))
                            .child(Input::new(&editor.name_input).small()),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_1()
                            .child(div().text_xs().text_color(p.muted).child("Description"))
                            .child(Input::new(&editor.desc_input).small()),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(p.muted)
                            .child("Instructions (saved as SKILL.md)"),
                    )
                    .child(Textarea::new(&editor.body_input)),
            )
            .when_some(self.skill_notice.as_ref(), |col, (msg, is_err)| {
                col.child(
                    div()
                        .text_xs()
                        .text_color(if *is_err { p.bad } else { p.muted })
                        .child(msg.clone()),
                )
            })
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("skill-draft-ai")
                            .xsmall()
                            .disabled(self.skill_drafting || !has_loaded_model)
                            .label(if self.skill_drafting {
                                "Drafting…"
                            } else if has_loaded_model {
                                "Draft with AI"
                            } else {
                                "Load a model to draft with AI"
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.draft_skill_with_ai(window, cx);
                            })),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("skill-cancel")
                                    .small()
                                    .label("Cancel")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.skill_editor = None;
                                        this.skill_notice = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("skill-save")
                                    .primary()
                                    .small()
                                    .label("Save skill")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.save_skill_editor(cx);
                                    })),
                            ),
                    ),
            )
    }

    fn loaded_explain_model(&self) -> Option<(String, String)> {
        let loaded = self.backend.app.models();
        if let Some(sel) = self.selected_model.as_deref()
            && let Some(m) = loaded
                .iter()
                .find(|m| m.id == sel && matches!(m.state, LoadState::Running { .. }))
        {
            return Some((m.id.clone(), m.name.clone()));
        }
        loaded
            .into_iter()
            .find(|m| matches!(m.state, LoadState::Running { .. }))
            .map(|m| (m.id, m.name))
    }

    fn ask_model_explain(
        &mut self,
        cache_key: String,
        command: String,
        focus_token: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if matches!(
            self.model_explanations.get(&cache_key),
            Some(ModelExplainState::Loading { .. })
        ) {
            return;
        }
        let Some((model_id, model_name)) = self.loaded_explain_model() else {
            self.model_explanations.insert(
                cache_key,
                ModelExplainState::Error(
                    "Load a model first to ask what this command means.".into(),
                ),
            );
            cx.notify();
            return;
        };

        self.model_explanations.insert(
            cache_key.clone(),
            ModelExplainState::Loading {
                model_name: model_name.clone(),
            },
        );
        cx.notify();

        let app = self.backend.app.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.backend.tokio.spawn(async move {
            let res = explain::ask_model_to_explain(
                &app,
                &command,
                focus_token.as_deref(),
                Some(&model_id),
            )
            .await
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(res);
        });

        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                let state = match result {
                    Ok(text) => ModelExplainState::Ready { model_name, text },
                    Err(err) => ModelExplainState::Error(err),
                };
                this.model_explanations.insert(cache_key, state);
                cx.notify();
            });
        })
        .detach();
    }

    fn render_explained_command(
        &self,
        block_id: usize,
        command: &str,
        p: crate::theme::Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let tokens = explain::parse_command(command);
        let full_command = command.to_string();
        let chat_entity = cx.entity().clone();
        let loaded_model_name = self.loaded_explain_model().map(|(_, name)| name);

        h_flex()
            .flex_1()
            .min_w_0()
            .flex_wrap()
            .items_center()
            .gap_y(px(2.0))
            .children(tokens.into_iter().enumerate().map(move |(tok_idx, tok)| {
                let space_left = if tok.space_before { px(5.0) } else { px(0.0) };
                let is_interactive =
                    tok.explanation.is_some() || tok.kind == TokenKind::Command;

                if !is_interactive {
                    return div()
                        .ml(space_left)
                        .text_xs()
                        .text_color(p.accent)
                        .child(tok.text)
                        .into_any_element();
                }

                let is_known = tok.explanation.is_some();
                let is_operator = tok.kind == TokenKind::Operator;
                let text_color = if is_operator {
                    p.warn
                } else if is_known {
                    p.accent
                } else {
                    p.text
                };
                let underline_color = if is_operator {
                    p.warn.opacity(0.75)
                } else if is_known {
                    p.accent.opacity(0.75)
                } else {
                    p.muted.opacity(0.85)
                };

                let tok_text = tok.text.clone();
                let tok_kind = tok.kind;
                let explanation = tok.explanation.clone();
                let full_cmd = full_command.clone();
                let entity = chat_entity.clone();
                let model_label = loaded_model_name.clone();
                let focus_for_model = (!is_known).then(|| tok_text.clone());
                let cache_key = match &focus_for_model {
                    Some(f) => format!("{full_cmd}\n{f}"),
                    None => full_cmd.clone(),
                };
                let popover_uid = block_id.saturating_mul(1024).saturating_add(tok_idx);

                let trigger = CommandTokenTrigger {
                    id: popover_uid,
                    text: tok_text.clone(),
                    space_left,
                    text_color,
                    underline_color,
                    hover_bg: p.chip,
                    selected: false,
                };

                Popover::new(("cmd-tok-pop", popover_uid))
                    .anchor(Anchor::BottomLeft)
                    .arrow(true)
                    .trigger(trigger)
                    .content(move |_, _, cx| {
                        let model_state = entity
                            .read(cx)
                            .model_explanations
                            .get(&cache_key)
                            .cloned();

                        let title = explanation
                            .as_ref()
                            .map(|e| e.title.clone())
                            .unwrap_or_else(|| tok_text.clone());
                        let badge = if is_known {
                            tok_kind.label()
                        } else {
                            "Unknown command"
                        };

                        let entity_for_btn = entity.clone();
                        let cache_key_btn = cache_key.clone();
                        let full_cmd_btn = full_cmd.clone();
                        let focus_btn = focus_for_model.clone();
                        let ask_label = match (&model_label, &focus_for_model) {
                            (Some(m), Some(f)) => format!("Ask {m} what `{f}` means"),
                            (Some(m), None) => format!("Ask {m} to explain full command"),
                            (None, _) => "Load a model to ask AI".into(),
                        };

                        v_flex()
                            .w(px(320.0))
                            .gap_2()
                            .child(
                                h_flex()
                                    .justify_between()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                            .child(title),
                                    )
                                    .child(Tag::secondary().small().child(badge)),
                            )
                            .when_some(explanation.clone(), |col, exp| {
                                col.child(
                                    div().text_xs().text_color(p.text).child(exp.summary),
                                )
                                .when(!exp.details.is_empty(), |col| {
                                    col.child(
                                        v_flex()
                                            .gap_1()
                                            .pl_2()
                                            .border_l_2()
                                            .border_color(p.line)
                                            .children(exp.details.into_iter().map(|d| {
                                                div().text_xs().text_color(p.muted).child(d)
                                            })),
                                    )
                                })
                                .when_some(exp.note, |col, note| {
                                    col.child(
                                        div()
                                            .px_2()
                                            .py_1()
                                            .rounded(px(4.0))
                                            .bg(p.warn.opacity(0.14))
                                            .text_xs()
                                            .text_color(p.warn)
                                            .child(note),
                                    )
                                })
                            })
                            .when(!is_known, |col| {
                                col.child(
                                    div().text_xs().text_color(p.muted).child(
                                        "Not in the built-in command guide yet. You can ask your local model in a separate side request without interrupting this chat.",
                                    ),
                                )
                            })
                            .child(
                                v_flex()
                                    .pt_2()
                                    .gap_1()
                                    .border_t_1()
                                    .border_color(p.line)
                                    .map(|col| match model_state {
                                        Some(ModelExplainState::Loading { model_name }) => col
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(p.muted)
                                                    .child(format!(
                                                        "Asking {model_name} in a separate request…"
                                                    )),
                                            ),
                                        Some(ModelExplainState::Ready { model_name, text }) => col
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                                                    .text_color(p.muted)
                                                    .child(format!("Explained by {model_name}")),
                                            )
                                            .child(
                                                div()
                                                    .p_2()
                                                    .rounded(px(6.0))
                                                    .bg(p.chip)
                                                    .text_xs()
                                                    .text_color(p.text)
                                                    .child(text),
                                            ),
                                        Some(ModelExplainState::Error(err)) => {
                                            let ent = entity_for_btn.clone();
                                            let ck = cache_key_btn.clone();
                                            let cmd = full_cmd_btn.clone();
                                            let foc = focus_btn.clone();
                                            col.child(
                                                div()
                                                    .text_xs()
                                                    .text_color(p.bad)
                                                    .child(err),
                                            )
                                            .child(
                                                Button::new(("cmd-ask-retry", popover_uid))
                                                    .xsmall()
                                                    .label("Try again")
                                                    .on_click(move |_, _, cx| {
                                                        ent.update(cx, |this, cx| {
                                                            this.ask_model_explain(
                                                                ck.clone(),
                                                                cmd.clone(),
                                                                foc.clone(),
                                                                cx,
                                                            );
                                                        });
                                                    }),
                                            )
                                        }
                                        None => {
                                            let ent = entity_for_btn.clone();
                                            let ck = cache_key_btn.clone();
                                            let cmd = full_cmd_btn.clone();
                                            let foc = focus_btn.clone();
                                            let has_loaded = model_label.is_some();
                                            col.child(
                                                Button::new(("cmd-ask-ai", popover_uid))
                                                    .when(!is_known && has_loaded, |b| b.primary())
                                                    .when(is_known || !has_loaded, |b| b.ghost())
                                                    .xsmall()
                                                    .disabled(!has_loaded)
                                                    .label(ask_label)
                                                    .on_click(move |_, _, cx| {
                                                        ent.update(cx, |this, cx| {
                                                            this.ask_model_explain(
                                                                ck.clone(),
                                                                cmd.clone(),
                                                                foc.clone(),
                                                                cx,
                                                            );
                                                        });
                                                    }),
                                            )
                                        }
                                    }),
                            )
                    })
                    .into_any_element()
            }))
    }

    fn render_empty_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_dark = cx.theme().mode == ThemeMode::Dark;
        let p = palette(is_dark);
        let proj = self
            .selected_project
            .as_deref()
            .and_then(|id| self.backend.app.project(id));

        let title = proj
            .as_ref()
            .map(|pr| format!("What should the agent do in {}?", pr.name))
            .unwrap_or_else(|| "Pick a project to get started".into());
        let subtitle = match &proj {
            Some(pr) => {
                let asks = match pr.approvals {
                    Approvals::Commands => "You'll be asked before commands run.",
                    Approvals::Everything => "You'll be asked before commands run and files change.",
                    Approvals::Never => {
                        "It won't ask before acting; the guard and sandbox still check everything."
                    }
                };
                format!(
                    "It can read and change files in {} and run commands in a sandbox. It can't see your passwords, keys, or other folders. {asks}",
                    pr.name
                )
            }
            None => "Choose a project folder so the agent knows where it may work.".into(),
        };

        v_flex()
            .mt(px(64.0))
            .mx_auto()
            .max_w(px(500.0))
            .items_center()
            .gap_3()
            .child(
                div()
                    .text_base()
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(p.muted)
                    .text_center()
                    .child(subtitle),
            )
            .when(proj.is_some(), |box_| {
                box_.child(
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
            })
            .when(proj.is_none(), |box_| {
                box_.child(
                    Button::new("empty-new-proj")
                        .primary()
                        .small()
                        .label("Choose project folder…")
                        .on_click(|_, _, cx| {
                            choose_project_folder(
                                "Choose a folder for the agent to work in",
                                true,
                                cx,
                            );
                        }),
                )
            })
    }

    fn render_block(&self, block: &ChatBlock, cx: &mut Context<Self>) -> impl IntoElement {
        let is_dark = cx.theme().mode == ThemeMode::Dark;
        let p = palette(is_dark);

        match block {
            ChatBlock::User { text, .. } => h_flex().w_full().flex_shrink_0().justify_end().child(
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
            ChatBlock::Thinking { id, text, open, .. } => {
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
            ChatBlock::Assistant {
                id,
                text,
                guard_notes,
                ..
            } => v_flex()
                .w_full()
                .flex_shrink_0()
                .gap_2()
                .when(!text.trim().is_empty(), |col| {
                    col.child(
                        div()
                            .w_full()
                            .text_sm()
                            .child(TextView::markdown(("md-msg", *id), text.clone()).selectable(true)),
                    )
                })
                .children(guard_notes.iter().map(|note| {
                    div()
                        .w_full()
                        .px_3()
                        .py_2()
                        .rounded(px(6.0))
                        .bg(p.warn.opacity(0.14))
                        .text_color(p.warn)
                        .text_xs()
                        .child(format_guard_note(note))
                })),
            ChatBlock::ToolCall(tc) => {
                let ToolCallBlock {
                    id,
                    kind,
                    verb,
                    subject,
                    status,
                    open,
                    diff,
                    output,
                    approval,
                    ..
                } = tc.as_ref();
                let block_id = *id;
                let arrow = if *open { "▾" } else { "▸" };
                let state_color = match status.as_str() {
                    "failed" => p.bad,
                    "declined" | "waiting for you" => p.warn,
                    _ => p.muted,
                };
                let project_name = self
                    .selected_project
                    .as_deref()
                    .and_then(|pid| self.backend.app.project(pid))
                    .map(|pr| pr.name)
                    .unwrap_or_else(|| "the project".into());
                let question = match kind.as_str() {
                    "execute" => "Run this command?",
                    "edit" => "Make this change?",
                    "delete" => "Delete this?",
                    "fetch" => "Fetch this page?",
                    _ => "Allow this?",
                };
                let why = match kind.as_str() {
                    "execute" => Some(format!(
                        "It runs in the sandbox: it can change files only in {project_name}, and the guard has checked it."
                    )),
                    "edit" => Some(format!("Only files in {project_name} can change.")),
                    "delete" => Some(format!("Only files in {project_name} can be deleted.")),
                    "fetch" => Some("The guard has checked the address.".into()),
                    _ => None,
                };

                let is_execute = kind == "execute" && !subject.is_empty();
                let header = if is_execute {
                    h_flex()
                        .px_3()
                        .py_2()
                        .gap_2()
                        .items_center()
                        .child(
                            h_flex()
                                .id(("tool-head-left", block_id))
                                .flex_shrink_0()
                                .cursor_pointer()
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
                                ),
                        )
                        .child(self.render_explained_command(block_id, subject, p, cx))
                        .child(
                            div()
                                .id(("tool-head-right", block_id))
                                .flex_shrink_0()
                                .cursor_pointer()
                                .text_xs()
                                .text_color(state_color)
                                .child(status.clone())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.toggle_block_open(block_id, cx);
                                })),
                        )
                        .into_any_element()
                } else {
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
                        .child(div().text_xs().text_color(state_color).child(status.clone()))
                        .into_any_element()
                };

                v_flex()
                    .w_full()
                    .flex_shrink_0()
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(if status == "failed" { p.bad } else { p.line })
                    .bg(p.panel)
                    .child(header)
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
                                            .id(("tool-out", block_id))
                                            .max_h(px(260.0))
                                            .overflow_y_scroll()
                                            .px_2()
                                            .py_1()
                                            .rounded(px(6.0))
                                            .bg(p.chip)
                                            .text_xs()
                                            .text_color(p.muted)
                                            .child(out),
                                    )
                                })
                                .when_some(approval.clone(), |body, pending| {
                                    let req_id = pending.request_id.clone();
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
                                                            .font_weight(
                                                                gpui_kit::FontWeight::SEMIBOLD,
                                                            )
                                                            .child(question),
                                                    )
                                                    .when_some(why.clone(), |col, w| {
                                                        col.child(
                                                            div()
                                                                .text_xs()
                                                                .text_color(p.muted)
                                                                .child(w),
                                                        )
                                                    }),
                                            )
                                            .children(pending.options.into_iter().enumerate().map(
                                                |(i, opt)| {
                                                    let is_primary = opt.kind == "allow_once";
                                                    let req = req_id.clone();
                                                    let opt_clone = opt.clone();
                                                    Button::new(("perm-opt", block_id * 16 + i))
                                                        .when(is_primary, |b| b.primary())
                                                        .small()
                                                        .label(opt.label)
                                                        .on_click(cx.listener(
                                                            move |this, _, _, cx| {
                                                                this.answer_permission(
                                                                    block_id,
                                                                    req.clone(),
                                                                    opt_clone.clone(),
                                                                    cx,
                                                                );
                                                            },
                                                        ))
                                                },
                                            )),
                                    )
                                }),
                        )
                    })
            }
            ChatBlock::StatusNote { text, is_error } => div()
                .w_full()
                .flex_shrink_0()
                .px_3()
                .py_2()
                .rounded(px(6.0))
                .bg(if *is_error {
                    p.bad.opacity(0.14)
                } else {
                    p.chip
                })
                .text_color(if *is_error { p.bad } else { p.muted })
                .text_xs()
                .child(text.clone()),
        }
    }

    fn render_session_row(
        &self,
        idx: usize,
        s: &SessionMeta,
        is_archived: bool,
        p: crate::theme::Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let sid_open = s.id.clone();
        let sid_arch = s.id.clone();
        let sid_del = s.id.clone();
        let title_del = s.title.clone();
        let active = self.active_session.as_deref() == Some(s.id.as_str());

        h_flex()
            .id(("session-row", idx))
            .flex_shrink_0()
            .gap_1()
            .px_2()
            .py(px(4.0))
            .items_center()
            .rounded(px(6.0))
            .when(active, |d| d.bg(p.chip))
            .hover(|d| d.bg(p.chip))
            .child(
                div()
                    .id(("session-open", idx))
                    .flex_1()
                    .min_w_0()
                    .cursor_pointer()
                    .text_sm()
                    .truncate()
                    .when(active, |d| d.font_weight(gpui_kit::FontWeight::MEDIUM))
                    .child(s.title.clone())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.show_chat(Some(sid_open.clone()), window, cx);
                    })),
            )
            .child(
                Button::new(("session-arch", idx))
                    .ghost()
                    .xsmall()
                    .label(if is_archived { "↩" } else { "⤓" })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.archive_chat(sid_arch.clone(), !is_archived, window, cx);
                    })),
            )
            .child(
                Button::new(("session-del", idx))
                    .ghost()
                    .xsmall()
                    .label("×")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.confirm_delete_chat = Some((sid_del.clone(), title_del.clone()));
                        cx.notify();
                    })),
            )
    }
}

impl Render for ChatWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_scroll_state();

        let is_dark = cx.theme().mode == ThemeMode::Dark;
        let p = palette(is_dark);

        let proj = self
            .selected_project
            .as_deref()
            .and_then(|id| self.backend.app.project(id));
        let project_path = proj
            .as_ref()
            .map(|pr| format!("· {}", pr.main().display()))
            .unwrap_or_default();

        let active_title = self
            .active_session
            .as_deref()
            .and_then(|id| self.sessions.iter().find(|s| s.id == id))
            .map(|s| s.title.clone())
            .unwrap_or_else(|| "New chat".into());

        let usage_ratio = self
            .usage
            .filter(|(_, size)| *size > 0)
            .map(|(used, size)| used as f64 / size as f64);
        let usage_text = usage_ratio.map(|r| format!("Chat {:.0}% full", r * 100.0));
        let is_long_chat =
            self.active_session.is_some() && !self.busy && usage_ratio.is_some_and(|r| r >= LONG_CHAT_RATIO);

        let loaded = self.backend.app.models();
        let loading_models: Vec<(String, String, f32, String)> = loaded
            .iter()
            .filter_map(|m| match &m.state {
                LoadState::Loading { progress, stage } => Some((
                    m.id.clone(),
                    m.name.clone(),
                    progress.unwrap_or(0.04).clamp(0.02, 0.99),
                    stage.clone().unwrap_or_else(|| "Loading model…".into()),
                )),
                _ => None,
            })
            .collect();
        let selected_is_running = self.selected_model.as_deref().is_some_and(|id| {
            loaded
                .iter()
                .any(|m| m.id == id && matches!(m.state, LoadState::Running { .. }))
        });
        let selected_loading = self.selected_model.as_deref().and_then(|id| {
            loading_models.iter().find(|(mid, _, _, _)| mid == id).cloned()
        });
        let has_loading_models = !loading_models.is_empty();
        let has_model = self.selected_model.is_some();
        let vm_blocked_builtin = proj
            .as_ref()
            .is_some_and(|pr| pr.isolation == Isolation::LinuxVm && self.engine == ChatEngine::Builtin);
        let can_send = !self.busy && proj.is_some() && has_model && !vm_blocked_builtin;

        let judge_loading = matches!(
            *self.backend.app.judge_status.lock().unwrap(),
            JudgeStatus::Loading
        );
        let waiting_for_user = self
            .blocks
            .iter()
            .any(|b| matches!(b, ChatBlock::ToolCall(tc) if tc.approval.is_some()));
        let elapsed = format_elapsed(
            self.working_since
                .map(|t| t.elapsed().as_secs())
                .unwrap_or(0),
        );
        let is_first_turn = self
            .blocks
            .iter()
            .filter(|b| matches!(b, ChatBlock::User { .. }))
            .count()
            <= 1;
        let working_text = if waiting_for_user {
            "Waiting for you: allow or deny the request above.".to_string()
        } else if let Some((_, name, frac, stage)) = &selected_loading {
            format!("Loading {name} — {stage} ({:.0}%)", frac * 100.0)
        } else if judge_loading {
            format!("Starting safety judge… {elapsed}")
        } else {
            match self.working_phase {
                WorkingPhase::StartingAgent => format!("Starting OpenCode… {elapsed}"),
                WorkingPhase::ReadingPrompt => {
                    if is_first_turn {
                        format!(
                            "Reading prompt & tools… {elapsed} (first turn builds prompt cache)"
                        )
                    } else {
                        format!("Reading prompt… {elapsed}")
                    }
                }
                WorkingPhase::Thinking => format!("Thinking… {elapsed}"),
                WorkingPhase::Writing => format!("Writing… {elapsed}"),
                WorkingPhase::RunningTool => format!("Running tool… {elapsed}"),
            }
        };

        let composer_text = self.composer.read(cx).value().to_string();
        let slash_query: Option<String> = if self.skill_editor.is_none() && !self.show_skills_panel {
            let trimmed = composer_text.trim_start();
            if trimmed.starts_with('/') && !trimmed.contains(char::is_whitespace) {
                Some(trimmed.to_string())
            } else if self.show_slash_menu {
                Some("/".to_string())
            } else {
                None
            }
        } else {
            None
        };

        let active_sessions: Vec<SessionMeta> = self
            .sessions
            .iter()
            .filter(|s| !self.archived.contains(&s.id))
            .cloned()
            .collect();
        let archived_sessions: Vec<SessionMeta> = self
            .sessions
            .iter()
            .filter(|s| self.archived.contains(&s.id))
            .cloned()
            .collect();
        let archived_count = archived_sessions.len();
        let archived_expanded = self.show_archived
            || archived_sessions
                .iter()
                .any(|s| self.active_session.as_deref() == Some(s.id.as_str()));
        let rendered_active_sessions: Vec<gpui_kit::AnyElement> = active_sessions
            .iter()
            .enumerate()
            .map(|(i, s)| self.render_session_row(i, s, false, p, cx).into_any_element())
            .collect();
        let rendered_archived_sessions: Vec<gpui_kit::AnyElement> = if archived_expanded {
            archived_sessions
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    self.render_session_row(10_000 + i, s, true, p, cx)
                        .into_any_element()
                })
                .collect()
        } else {
            Vec::new()
        };

        let is_empty = self.blocks.is_empty();
        let rendered_blocks: Vec<gpui_kit::AnyElement> = self
            .blocks
            .iter()
            .map(|block| self.render_block(block, cx).into_any_element())
            .collect();

        let active_sid_for_arch = self.active_session.clone();
        let active_not_archived = active_sid_for_arch
            .as_deref()
            .is_some_and(|id| !self.archived.contains(id));

        h_flex()
            .size_full()
            .bg(p.bg)
            .text_color(p.text)
            // Sidebar
            .child(
                v_flex()
                    .w(px(244.0))
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
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.show_chat(None, window, cx);
                                })),
                        ),
                    )
                    .when_some(self.confirm_delete_chat.clone(), |col, (cid, ctitle)| {
                        col.child(
                            v_flex()
                                .flex_shrink_0()
                                .p_2()
                                .gap_2()
                                .rounded(px(6.0))
                                .bg(p.bad.opacity(0.14))
                                .child(
                                    div()
                                        .text_xs()
                                        .child(format!("Delete \"{ctitle}\"?")),
                                )
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .child(
                                            Button::new("confirm-del-chat")
                                                .danger()
                                                .xsmall()
                                                .label("Delete")
                                                .on_click(cx.listener(move |this, _, window, cx| {
                                                    this.delete_chat(cid.clone(), window, cx);
                                                })),
                                        )
                                        .child(
                                            Button::new("cancel-del-chat")
                                                .xsmall()
                                                .label("Cancel")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.confirm_delete_chat = None;
                                                    cx.notify();
                                                })),
                                        ),
                                ),
                        )
                    })
                    .child(
                        div()
                            .id("chat-list")
                            .flex_1()
                            .min_h_0()
                            .w_full()
                            .overflow_y_scroll()
                            .mt_1()
                            .child(
                                v_flex()
                                    .w_full()
                                    .gap_1()
                                    .when_some(self.starting_note.clone(), |col, note| {
                                        col.child(
                                            div()
                                                .px_2()
                                                .py_1()
                                                .text_xs()
                                                .text_color(p.muted)
                                                .child(note),
                                        )
                                    })
                                    .when_some(self.sessions_error.clone(), |col, err| {
                                        col.child(
                                            div()
                                                .px_2()
                                                .py_1()
                                                .text_xs()
                                                .text_color(p.bad)
                                                .child(err),
                                        )
                                    })
                                    .children(rendered_active_sessions),
                            ),
                    )
                    .when(archived_count > 0, |col| {
                        col.child(
                            v_flex()
                                .flex_shrink_0()
                                .pt_2()
                                .gap_1()
                                .border_t_1()
                                .border_color(p.line)
                                .child(
                                    div()
                                        .id("toggle-archived")
                                        .cursor_pointer()
                                        .px_2()
                                        .py_1()
                                        .text_xs()
                                        .text_color(p.muted)
                                        .child(format!(
                                            "{} Archived ({archived_count})",
                                            if archived_expanded { "▾" } else { "▸" }
                                        ))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.show_archived = !this.show_archived;
                                            cx.notify();
                                        })),
                                )
                                .when(archived_expanded, |box_| {
                                    box_.child(
                                        div()
                                            .id("archived-list")
                                            .max_h(px(180.0))
                                            .w_full()
                                            .overflow_y_scroll()
                                            .child(
                                                v_flex()
                                                    .w_full()
                                                    .gap_1()
                                                    .children(rendered_archived_sessions),
                                            ),
                                    )
                                }),
                        )
                    })
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
                                            .truncate()
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
                                    .when(active_not_archived, |row| {
                                        let sid = active_sid_for_arch.clone().unwrap();
                                        row.child(
                                            Button::new("header-archive")
                                                .ghost()
                                                .small()
                                                .label("Archive")
                                                .on_click(cx.listener(move |this, _, window, cx| {
                                                    this.archive_chat(sid.clone(), true, window, cx);
                                                })),
                                        )
                                    })
                                    .when_some(usage_text, |row, usage| {
                                        row.child(Tag::secondary().small().child(usage))
                                    })
                                    .child(
                                        div()
                                            .w(px(192.0))
                                            .h(px(28.0))
                                            .flex_shrink_0()
                                            .child(Select::new(&self.model_select).small()),
                                    )
                                    .when(has_model, |row| {
                                        row.child(
                                            div()
                                                .w(px(122.0))
                                                .h(px(28.0))
                                                .flex_shrink_0()
                                                .child(Select::new(&self.reasoning_select).small()),
                                        )
                                    })
                                    .when(selected_is_running, |row| {
                                        let mid = self.selected_model.clone();
                                        row.child(
                                            Button::new("header-unload")
                                                .small()
                                                .label("Unload")
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    if this.busy {
                                                        this.stop_current(cx);
                                                    }
                                                    this.backend.spawn_unload_model(mid.clone());
                                                })),
                                        )
                                    }),
                            ),
                    )
                    // Model loading progress bar banner
                    .when(has_loading_models, |col| {
                        col.child(
                            v_flex()
                                .flex_shrink_0()
                                .px_4()
                                .py_2()
                                .gap_2()
                                .bg(p.panel)
                                .border_b_1()
                                .border_color(p.line)
                                .children(loading_models.into_iter().enumerate().map(
                                    |(i, (mid, name, frac, stage))| {
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
                                                        Button::new(("chat-cancel-load", i))
                                                            .xsmall()
                                                            .label("Cancel")
                                                            .on_click(cx.listener(
                                                                move |this, _, _, _| {
                                                                    this.backend
                                                                        .spawn_unload_model(Some(
                                                                            mid.clone(),
                                                                        ));
                                                                },
                                                            )),
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
                                    },
                                )),
                        )
                    })
                    // Safety judge startup banner (when judge is loading right after chat model)
                    .when(judge_loading && !has_loading_models, |col| {
                        col.child(
                            v_flex()
                                .flex_shrink_0()
                                .px_4()
                                .py_2()
                                .gap_1()
                                .bg(p.panel)
                                .border_b_1()
                                .border_color(p.line)
                                .child(
                                    div()
                                        .text_xs()
                                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                                        .text_color(p.muted)
                                        .child("Starting safety judge…"),
                                )
                                .child(
                                    div()
                                        .w_full()
                                        .h(px(4.0))
                                        .rounded_full()
                                        .bg(p.chip)
                                        .overflow_hidden()
                                        .child(
                                            div()
                                                .h_full()
                                                .w(gpui_kit::relative(0.65))
                                                .bg(p.accent.opacity(0.7)),
                                        ),
                                ),
                        )
                    })
                    // Notice banners (missing model, room confirmation, or >=60% context warning)
                    .when(!has_model, |col| {
                        col.child(
                            h_flex()
                                .flex_shrink_0()
                                .px_4()
                                .py_2()
                                .gap_2()
                                .items_center()
                                .justify_between()
                                .bg(p.chip)
                                .border_b_1()
                                .border_color(p.line)
                                .child(div().text_xs().child("Download a model to chat."))
                                .child(
                                    Button::new("banner-models")
                                        .primary()
                                        .small()
                                        .label("Models…")
                                        .on_click(|_, _, cx| open_settings(Some(0), cx)),
                                ),
                        )
                    })
                    .when(vm_blocked_builtin, |col| {
                        col.child(
                            h_flex()
                                .flex_shrink_0()
                                .px_4()
                                .py_2()
                                .gap_2()
                                .items_center()
                                .justify_between()
                                .bg(p.warn.opacity(0.14))
                                .border_b_1()
                                .border_color(p.line)
                                .child(div().text_xs().child(
                                    "Linux VM projects require OpenCode; switch this project to macOS sandbox in Settings or install OpenCode.",
                                )),
                        )
                    })
                    .when_some(self.pending_send.as_ref(), |col, pending| {
                        let display_text = pending.display_text.clone();
                        let prompt_text = pending.prompt_text.clone();
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
                                .flex_shrink_0()
                                .px_4()
                                .py_2()
                                .gap_2()
                                .items_center()
                                .bg(p.warn.opacity(0.14))
                                .border_b_1()
                                .border_color(p.warn)
                                .child(div().flex_1().text_xs().child(format!(
                                    "Loading {} needs the memory {names} {verb}. Unload {names} and send?",
                                    pending.model_name
                                )))
                                .child(
                                    Button::new("confirm-room-send")
                                        .primary()
                                        .small()
                                        .label("Unload & send")
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.composer.update(cx, |s, cx| {
                                                s.set_value("", window, cx);
                                            });
                                            this.dispatch_send(
                                                display_text.clone(),
                                                prompt_text.clone(),
                                                mid.clone(),
                                                unload_ids.clone(),
                                                window,
                                                cx,
                                            );
                                        })),
                                )
                                .child(
                                    Button::new("cancel-room-send")
                                        .small()
                                        .label("Cancel")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.pending_send = None;
                                            cx.notify();
                                        })),
                                ),
                        )
                    })
                    .when(is_long_chat, |col| {
                        let pct = (usage_ratio.unwrap_or(0.0) * 100.0).round() as u64;
                        let sid = self.active_session.clone().unwrap();
                        col.child(
                            h_flex()
                                .flex_shrink_0()
                                .px_4()
                                .py_2()
                                .gap_2()
                                .items_center()
                                .bg(p.chip)
                                .border_b_1()
                                .border_color(p.line)
                                .child(div().flex_1().text_xs().text_color(p.muted).child(format!(
                                    "This chat is {pct}% full. Long chats get slower and less reliable. If this task is done, start a new chat."
                                )))
                                .child(
                                    Button::new("archive-long-chat")
                                        .small()
                                        .label("Archive and start a new chat")
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.archive_chat(sid.clone(), true, window, cx);
                                        })),
                                ),
                        )
                    })
                    // Message stream
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
                    // Working status line (+ Jump to bottom when scrolled up)
                    .when(self.busy || (!self.auto_scroll && !is_empty), |col| {
                        col.child(
                            h_flex()
                                .flex_shrink_0()
                                .px_5()
                                .py_1()
                                .gap_2()
                                .items_center()
                                .justify_between()
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .items_center()
                                        .when(self.busy, |row| {
                                            row.child(
                                                div()
                                                    .size(px(8.0))
                                                    .rounded_full()
                                                    .bg(if waiting_for_user {
                                                        p.warn
                                                    } else {
                                                        p.accent
                                                    }),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(if waiting_for_user {
                                                        p.warn
                                                    } else {
                                                        p.muted
                                                    })
                                                    .child(working_text),
                                            )
                                        }),
                                )
                                .when(!self.auto_scroll && !is_empty, |row| {
                                    row.child(
                                        Button::new("jump-bottom")
                                            .ghost()
                                            .xsmall()
                                            .label("↓ Jump to bottom")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.jump_to_bottom();
                                                cx.notify();
                                            })),
                                    )
                                }),
                        )
                    })
                    // Slash autocomplete, Skills Manager, or Skill Creator drawer above composer
                    .when_some(slash_query, |col, q| {
                        col.child(self.render_slash_menu(&q, p, cx))
                    })
                    .when(self.show_skills_panel, |col| {
                        col.child(self.render_skills_panel(p, cx))
                    })
                    .when_some(self.skill_editor.as_ref(), |col, editor| {
                        col.child(self.render_skill_editor(editor, p, cx))
                    })
                    // Active skill pill (if a skill is armed for the next message)
                    .when_some(self.active_skill.as_ref(), |col, skill| {
                        col.child(
                            h_flex()
                                .flex_shrink_0()
                                .px_4()
                                .py_1()
                                .gap_2()
                                .items_center()
                                .bg(p.panel)
                                .border_t_1()
                                .border_color(p.line)
                                .child(
                                    Tag::secondary()
                                        .small()
                                        .child(format!("Skill: /{}", skill.name)),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_xs()
                                        .text_color(p.muted)
                                        .truncate()
                                        .child(skill.description.clone()),
                                )
                                .child(
                                    Button::new("clear-active-skill")
                                        .ghost()
                                        .xsmall()
                                        .label("×")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.active_skill = None;
                                            cx.notify();
                                        })),
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
                            .child(
                                Button::new("slash-trigger-btn")
                                    .ghost()
                                    .small()
                                    .label("/")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.refresh_skills();
                                        this.show_skills_panel = false;
                                        this.skill_editor = None;
                                        this.show_slash_menu = !this.show_slash_menu;
                                        cx.notify();
                                    })),
                            )
                            .child(div().flex_1().child(Textarea::new(&self.composer)))
                            .child(if self.busy {
                                Button::new("stop-btn")
                                    .danger()
                                    .label("Stop")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.stop_current(cx);
                                    }))
                            } else {
                                Button::new("send-btn")
                                    .primary()
                                    .label("Send")
                                    .disabled(!can_send)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.send_current(window, cx);
                                    }))
                            }),
                    ),
            )
    }
}

fn content_text(content: &Value) -> String {
    if content["type"].as_str() == Some("text") {
        content["text"].as_str().unwrap_or("").to_string()
    } else {
        String::new()
    }
}

fn split_guard_notes(text: &str) -> (String, Vec<String>) {
    let mut clean = Vec::new();
    let mut notes = Vec::new();
    for line in text.split('\n') {
        if line.starts_with("[guard]") {
            notes.push(line.to_string());
        } else {
            clean.push(line);
        }
    }
    (clean.join("\n"), notes)
}

fn format_guard_note(line: &str) -> String {
    if let Some(rest) = line.strip_prefix("[guard] blocked call to ") {
        format!("The guard blocked a call to {rest}")
    } else if let Some(rest) = line.strip_prefix("[guard]") {
        format!("The guard: {}", rest.trim())
    } else {
        line.to_string()
    }
}

fn tool_verb(kind: &str, raw: &Value) -> String {
    match kind {
        "read" => "Read",
        "edit" => "Edit",
        "delete" => "Delete",
        "move" => "Move",
        "search" => "Search",
        "execute" => "Run",
        "fetch" => "Fetch",
        "think" => "Think",
        "switch_mode" => "Switch mode",
        _ => raw["name"]
            .as_str()
            .or_else(|| raw["title"].as_str())
            .unwrap_or("Tool"),
    }
    .to_string()
}

fn tool_subject(kind: &str, raw: &Value, main: Option<&str>) -> String {
    let input = &raw["rawInput"];
    let title = raw["title"].as_str().unwrap_or("");
    let where_path = raw["locations"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|loc| loc["path"].as_str());
    let relative = |path: &str| -> String {
        if let Some(m) = main {
            let prefix = format!("{m}/");
            if let Some(rel) = path.strip_prefix(&prefix) {
                return rel.to_string();
            }
        }
        path.to_string()
    };
    match kind {
        "execute" => input["command"].as_str().unwrap_or(title).to_string(),
        "search" => input["pattern"]
            .as_str()
            .or_else(|| input["query"].as_str())
            .unwrap_or(title)
            .to_string(),
        "fetch" => input["url"].as_str().unwrap_or(title).to_string(),
        "read" | "edit" | "delete" | "move" => input["path"]
            .as_str()
            .or_else(|| input["filePath"].as_str())
            .or(where_path)
            .map(relative)
            .unwrap_or_else(|| title.to_string()),
        _ => {
            if !title.is_empty() && raw["name"].as_str() != Some(title) {
                title.to_string()
            } else {
                String::new()
            }
        }
    }
}

fn compute_diff(old_text: &str, new_text: &str) -> Vec<DiffLine> {
    let a: Vec<&str> = old_text.split('\n').collect();
    let b: Vec<&str> = new_text.split('\n').collect();
    let mut start = 0;
    while start < a.len() && start < b.len() && a[start] == b[start] {
        start += 1;
    }
    let mut end_a = a.len();
    let mut end_b = b.len();
    while end_a > start && end_b > start && a[end_a - 1] == b[end_b - 1] {
        end_a -= 1;
        end_b -= 1;
    }
    let from = start.saturating_sub(2);
    let mut out = Vec::new();
    for line in &a[from..start] {
        out.push(DiffLine {
            kind: DiffKind::Same,
            text: (*line).to_string(),
        });
    }
    for line in &a[start..end_a] {
        out.push(DiffLine {
            kind: DiffKind::Del,
            text: (*line).to_string(),
        });
    }
    for line in &b[start..end_b] {
        out.push(DiffLine {
            kind: DiffKind::Add,
            text: (*line).to_string(),
        });
    }
    let tail_end = (end_b + 2).min(b.len());
    for line in &b[end_b..tail_end] {
        out.push(DiffLine {
            kind: DiffKind::Same,
            text: (*line).to_string(),
        });
    }
    out
}

fn parse_permission_options(options: &Value) -> Vec<PermissionOption> {
    let items: Vec<&Value> = options.as_array().into_iter().flatten().collect();
    let has_reject_once = items
        .iter()
        .any(|o| o["kind"].as_str() == Some("reject_once"));
    items
        .into_iter()
        .filter(|o| o["kind"].as_str() != Some("reject_always") || !has_reject_once)
        .filter_map(|o| {
            let option_id = o["optionId"].as_str()?.to_string();
            let kind = o["kind"].as_str().unwrap_or("").to_string();
            let label = match kind.as_str() {
                "allow_once" => "Allow",
                "allow_always" => "Allow for this chat",
                "reject_once" => "Deny",
                "reject_always" => "Never",
                _ => o["name"].as_str().unwrap_or("Choose"),
            }
            .to_string();
            Some(PermissionOption {
                option_id,
                kind,
                label,
            })
        })
        .collect()
}

fn reasoning_key(level: ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::Default => "default",
        ReasoningLevel::Off => "off",
        ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
    }
}

fn parse_reasoning_key(key: &str) -> ReasoningLevel {
    match key {
        "off" => ReasoningLevel::Off,
        "low" => ReasoningLevel::Low,
        "medium" => ReasoningLevel::Medium,
        "high" => ReasoningLevel::High,
        _ => ReasoningLevel::Default,
    }
}

fn acp_update_phase(update: &Value) -> Option<WorkingPhase> {
    match update["sessionUpdate"].as_str().unwrap_or("") {
        "agent_thought_chunk" => Some(WorkingPhase::Thinking),
        "agent_message_chunk" => Some(WorkingPhase::Writing),
        "tool_call" | "tool_call_update" => match update["status"].as_str().unwrap_or("") {
            "completed" | "failed" => Some(WorkingPhase::ReadingPrompt),
            _ => Some(WorkingPhase::RunningTool),
        },
        _ => None,
    }
}

fn format_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {}s", secs / 60, secs % 60)
    }
}
