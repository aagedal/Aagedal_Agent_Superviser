//! Native GPUI Chat window wired end-to-end to `superviser-backend`:
//! - OpenCode ACP (`opencode acp`) when available, with automatic fallback to the built-in agent
//! - Real session list, replay, archiving, deletion, and per-chat model persistence
//! - Live streaming Markdown (`TextView::markdown`), collapsible Thinking & Tool call cards,
//!   +/- diffs, real Allow/Allow-for-chat/Deny permission prompts, and Guard block notices
//! - Automatic model loading & room-check confirmation before sending, plus context-usage warning

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui_kit::assets::IconName;
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
    Anchor, App, AppContext, ClipboardEntry, ClipboardItem, Context, Div, Entity, ExternalPaths,
    FocusHandle, Focusable, Hsla, Image, ImageFormat, InteractiveElement, IntoElement,
    KeyDownEvent, ObjectFit, ParentElement, PathPromptOptions, Pixels, Render, ScrollHandle,
    SharedString, Stateful, StatefulInteractiveElement, Styled, StyledImage, Subscription, Window,
    div, img, point, px,
};
use serde_json::Value;
use superviser_backend::attachments::{self, ImageAttachment, MAX_IMAGES_PER_MESSAGE};
use superviser_backend::explain::{self, TokenKind};
use superviser_backend::files;
use superviser_backend::git::{self, GitStatus};
use superviser_backend::skills::{self, Skill, SkillScope};
use superviser_backend::subagents::{self, SubagentRecord, SubagentState};
use superviser_backend::{AcpEvent, AppEvent, ChatEvent, JudgeStatus, LoadState, ReasoningLevel};
use superviser_core::project::Approvals;
use superviser_core::sandbox::Isolation;
use entropy::Chat;

use crate::backend::Backend;
use crate::theme::palette;
use crate::windows::{choose_project_folder, open_project_settings, open_settings, open_setup};
use crate::{
    AddProject, ArchiveActiveChat, AttachImage, Dismiss, FocusChatSearch, FocusComposer,
    FocusModelPicker, FocusProjectPicker, FocusReasoningPicker, NewChat, NextChat, PrevChat,
    StopGeneration, ToggleGitCommit, ToggleShortcutsHelp,
};

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
    project_id: String,
    project_name: String,
    isolation: Isolation,
    title: String,
    updated_at: Option<u64>,
    model_name: Option<String>,
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
struct AttachedImage {
    id: usize,
    attachment: ImageAttachment,
    preview: Option<Arc<Image>>,
}

impl AttachedImage {
    fn from_attachment(id: usize, attachment: ImageAttachment) -> Self {
        let preview = ImageFormat::from_mime_type(&attachment.mime_type).and_then(|format| {
            let bytes = attachment.decode_bytes().ok()?;
            Some(Arc::new(Image::from_bytes(format, bytes)))
        });
        Self {
            id,
            attachment,
            preview,
        }
    }

    fn from_gpui_image(id: usize, image: &Image, name: String) -> anyhow::Result<Self> {
        let attachment =
            ImageAttachment::from_bytes(name, image.format.mime_type(), &image.bytes)?;
        Ok(Self {
            id,
            attachment,
            preview: Some(Arc::new(image.clone())),
        })
    }
}

#[derive(Clone)]
enum ChatBlock {
    User {
        key: Option<String>,
        text: String,
        images: Vec<AttachedImage>,
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
    images: Vec<AttachedImage>,
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
    _subs: Vec<Subscription>,
}

enum SlashMenuItem {
    Skill(Skill),
    NewSkill,
    ListSkills,
}

enum GitPanel {
    Commit {
        message_input: Entity<InputState>,
        _sub: Subscription,
    },
    Pr {
        title_input: Entity<InputState>,
        body_input: Entity<TextareaState>,
        draft: bool,
        _sub: Subscription,
    },
}

#[derive(Clone)]
struct GitNotice {
    text: String,
    url: Option<String>,
    is_error: bool,
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
    focus_handle: FocusHandle,
    session_focus_handles: HashMap<String, FocusHandle>,
    engine: ChatEngine,
    selected_project: Option<String>,
    filter_project: Option<String>,
    show_projects_popout: bool,
    projects_popout_search: Entity<InputState>,
    _projects_popout_sub: Subscription,
    projects_popout_selected: usize,
    show_new_chat_modal: bool,
    new_chat_search: Entity<InputState>,
    _new_chat_sub: Subscription,
    new_chat_selected: usize,
    model_select: Entity<SelectState<SearchableVec<PickerOption>>>,
    model_sub: Subscription,
    reasoning_select: Entity<SelectState<SearchableVec<PickerOption>>>,
    reasoning_sub: Subscription,
    selected_model: Option<String>,
    sessions: Vec<SessionMeta>,
    archived: HashSet<(String, String)>,
    show_archived: bool,
    sidebar_search: Entity<InputState>,
    _sidebar_search_sub: Subscription,
    active_session: Option<String>,
    blocks: Vec<ChatBlock>,
    usage: Option<(u64, u64)>, // (used, size)
    starting_note: Option<String>,
    sessions_error: Option<String>,
    pending_send: Option<PendingSend>,
    pending_images: Vec<AttachedImage>,
    attach_notice: Option<String>,
    next_image_id: usize,
    confirm_delete_chat: Option<(String, String, String)>, // (project_id, chat_id, title)
    composer: Entity<TextareaState>,
    scroll_handle: ScrollHandle,
    auto_scroll: bool,
    last_offset_y: Pixels,
    busy: bool,
    working_since: Option<Instant>,
    working_phase: WorkingPhase,
    working_since_by_session: HashMap<String, Instant>,
    working_phase_by_session: HashMap<String, WorkingPhase>,
    approval_pending_sessions: HashSet<String>,
    done_sessions: HashSet<String>,
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
    slash_selected: usize,
    slash_dismissed_query: Option<String>,
    show_skills_panel: bool,
    skill_editor: Option<SkillEditor>,
    skill_drafting: bool,
    skill_notice: Option<(String, bool)>,
    git_status: Option<GitStatus>,
    git_gen: u64,
    git_panel: Option<GitPanel>,
    git_busy: Option<String>,
    git_drafting: bool,
    git_notice: Option<GitNotice>,
    subagents: Vec<SubagentRecord>,
    inspected_subagent: Option<String>,
    show_shortcuts_help: bool,
    _composer_sub: Subscription,
}

impl Focusable for ChatWindow {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ChatWindow {
    pub fn new(
        backend: Arc<Backend>,
        initial_project: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let engine = ChatEngine::Builtin;

        let projects = backend.app.settings.lock().unwrap().projects.clone();
        let selected_project = initial_project
            .filter(|id| projects.iter().any(|p| &p.id == id))
            .or_else(|| projects.first().map(|p| p.id.clone()));

        let projects_popout_search =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search projects…"));
        let projects_popout_sub = cx.subscribe_in(
            &projects_popout_search,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    this.projects_popout_selected = 0;
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => {
                    this.confirm_projects_popout_selection(window, cx);
                }
                _ => {}
            },
        );

        let new_chat_search =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search projects…"));
        let new_chat_sub = cx.subscribe_in(
            &new_chat_search,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    this.new_chat_selected = 0;
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => {
                    this.confirm_new_chat_selection(window, cx);
                }
                _ => {}
            },
        );

        let selected_model = Self::default_model(&backend);
        let (model_select, model_sub) =
            Self::make_model_select(&backend, selected_model.as_deref(), window, cx);
        let (reasoning_select, reasoning_sub) =
            Self::make_reasoning_select(&backend, selected_model.as_deref(), window, cx);

        let sidebar_search = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Search chats…")
        });
        let sidebar_search_sub = cx.subscribe_in(
            &sidebar_search,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => {
                    let (active, archived) = this.filtered_sessions(cx);
                    if let Some(first) = active.into_iter().chain(archived).next() {
                        this.open_session(first.project_id, first.id, window, cx);
                    }
                }
                _ => {}
            },
        );

        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Ask the agent… Drop/paste images or type / for skills")
                .rows(2)
                .auto_grow(2, 6)
        });
        composer.update(cx, |s, cx| s.focus(window, cx));

        let composer_sub = cx.subscribe_in(
            &composer,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { shift: false, .. } => {
                    this.send_current(window, cx);
                }
                InputEvent::Change => {
                    this.show_slash_menu = false;
                    this.slash_selected = 0;
                    if let Some(sid) = this.active_session.as_deref() {
                        this.done_sessions.remove(sid);
                    }
                    let val = this.composer.read(cx).value().trim_start().to_string();
                    if this.slash_dismissed_query.as_deref() != Some(val.as_str()) {
                        this.slash_dismissed_query = None;
                    }
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

        // 1-second tick for the elapsed "Working… Xs" and live sub-agent timers.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let alive = this
                    .update(cx, |this, cx| {
                        let any_subagent_running =
                            this.subagents.iter().any(|s| s.state.is_active());
                        if this.busy
                            || !this.working_since_by_session.is_empty()
                            || any_subagent_running
                        {
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
            focus_handle,
            session_focus_handles: HashMap::new(),
            engine,
            selected_project: selected_project.clone(),
            filter_project: None,
            show_projects_popout: false,
            projects_popout_search,
            _projects_popout_sub: projects_popout_sub,
            projects_popout_selected: 0,
            show_new_chat_modal: false,
            new_chat_search,
            _new_chat_sub: new_chat_sub,
            new_chat_selected: 0,
            model_select,
            model_sub,
            reasoning_select,
            reasoning_sub,
            selected_model,
            sessions: Vec::new(),
            archived: HashSet::new(),
            show_archived: false,
            sidebar_search,
            _sidebar_search_sub: sidebar_search_sub,
            active_session: None,
            blocks: Vec::new(),
            usage: None,
            starting_note: None,
            sessions_error: None,
            pending_send: None,
            pending_images: Vec::new(),
            attach_notice: None,
            next_image_id: 1,
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
            approval_pending_sessions: HashSet::new(),
            done_sessions: HashSet::new(),
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
            slash_selected: 0,
            slash_dismissed_query: None,
            show_skills_panel: false,
            skill_editor: None,
            skill_drafting: false,
            skill_notice: None,
            git_status: None,
            git_gen: 0,
            git_panel: None,
            git_busy: None,
            git_drafting: false,
            git_notice: None,
            subagents: Vec::new(),
            inspected_subagent: None,
            show_shortcuts_help: false,
            _composer_sub: composer_sub,
        };

        this.refresh_skills();
        if selected_project.is_some() {
            this.refresh_sessions(cx);
            this.refresh_git(true, cx);
        }
        this
    }

    pub fn focus_composer(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.composer.update(cx, |s, cx| s.focus(window, cx));
    }

    fn sync_session_focus_handles(&mut self, cx: &mut Context<Self>) {
        let valid: HashSet<&str> = self.sessions.iter().map(|s| s.id.as_str()).collect();
        self.session_focus_handles
            .retain(|id, _| valid.contains(id.as_str()));
        for s in &self.sessions {
            self.session_focus_handles
                .entry(s.id.clone())
                .or_insert_with(|| cx.focus_handle());
        }
    }

    fn is_archived(&self, project_id: &str, session_id: &str) -> bool {
        self.archived
            .contains(&(project_id.to_string(), session_id.to_string()))
    }

    fn filtered_sessions(&self, cx: &App) -> (Vec<SessionMeta>, Vec<SessionMeta>) {
        let query = self
            .sidebar_search
            .read(cx)
            .value()
            .trim()
            .to_ascii_lowercase();
        let mut active = Vec::new();
        let mut archived = Vec::new();
        for s in &self.sessions {
            if let Some(filter_pid) = self.filter_project.as_deref()
                && s.project_id != filter_pid
            {
                continue;
            }
            if !query.is_empty() {
                let matches = s.title.to_ascii_lowercase().contains(&query)
                    || s.project_name.to_ascii_lowercase().contains(&query)
                    || s.model_name
                        .as_deref()
                        .is_some_and(|m| m.to_ascii_lowercase().contains(&query));
                if !matches {
                    continue;
                }
            }
            if self.is_archived(&s.project_id, &s.id) {
                archived.push(s.clone());
            } else {
                active.push(s.clone());
            }
        }
        (active, archived)
    }

    fn visible_sessions(&self, cx: &App) -> Vec<SessionMeta> {
        let (mut active, archived) = self.filtered_sessions(cx);
        let archived_expanded = self.show_archived
            || archived
                .iter()
                .any(|s| self.active_session.as_deref() == Some(s.id.as_str()));
        if archived_expanded {
            active.extend(archived);
        }
        active
    }

    fn switch_adjacent_chat(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let visible = self.visible_sessions(cx);
        if visible.is_empty() {
            return;
        }
        let next_idx = match self
            .active_session
            .as_deref()
            .and_then(|id| visible.iter().position(|x| x.id == id))
        {
            Some(cur) => {
                let len = visible.len() as isize;
                ((cur as isize + delta).rem_euclid(len)) as usize
            }
            None => {
                if delta >= 0 {
                    0
                } else {
                    visible.len() - 1
                }
            }
        };
        if let Some(target) = visible.get(next_idx).cloned() {
            self.open_session(target.project_id, target.id, window, cx);
        }
    }

    fn focus_adjacent_session_row(
        &self,
        current_id: &str,
        delta: isize,
        window: &mut Window,
        cx: &mut App,
    ) {
        let visible = self.visible_sessions(cx);
        if visible.is_empty() {
            return;
        }
        let Some(cur) = visible.iter().position(|s| s.id == current_id) else {
            return;
        };
        let len = visible.len() as isize;
        let next = ((cur as isize + delta).rem_euclid(len)) as usize;
        if let Some(fh) = self.session_focus_handles.get(&visible[next].id) {
            fh.focus(window, cx);
        }
    }

    fn focus_edge_session_row(&self, first: bool, window: &mut Window, cx: &mut App) {
        let visible = self.visible_sessions(cx);
        let target = if first { visible.first() } else { visible.last() };
        if let Some(s) = target
            && let Some(fh) = self.session_focus_handles.get(&s.id)
        {
            fh.focus(window, cx);
        }
    }

    fn active_slash_query(&self, cx: &App) -> Option<String> {
        if self.skill_editor.is_some() || self.show_skills_panel {
            return None;
        }
        let composer_text = self.composer.read(cx).value().to_string();
        let trimmed = composer_text.trim_start();
        if trimmed.starts_with('/') && !trimmed.contains(char::is_whitespace) {
            if self.slash_dismissed_query.as_deref() == Some(trimmed) {
                None
            } else {
                Some(trimmed.to_string())
            }
        } else if self.show_slash_menu {
            Some("/".to_string())
        } else {
            None
        }
    }

    fn slash_menu_items(&self, query: &str) -> Vec<SlashMenuItem> {
        let q = query.trim_start_matches('/').to_ascii_lowercase();
        let mut items: Vec<SlashMenuItem> = self
            .skills
            .iter()
            .filter(|s| {
                q.is_empty()
                    || s.name.contains(&q)
                    || s.description.to_ascii_lowercase().contains(&q)
            })
            .cloned()
            .map(SlashMenuItem::Skill)
            .collect();
        if q.is_empty() || "new-skill".contains(&q) || "create-skill".contains(&q) {
            items.push(SlashMenuItem::NewSkill);
        }
        if q.is_empty() || "skills".contains(&q) {
            items.push(SlashMenuItem::ListSkills);
        }
        items
    }

    fn activate_slash_item(
        &mut self,
        item: SlashMenuItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.composer.read(cx).value().trim_start().starts_with('/') {
            self.composer
                .update(cx, |s, cx| s.set_value("", window, cx));
        }
        self.show_slash_menu = false;
        self.slash_selected = 0;
        self.slash_dismissed_query = None;
        match item {
            SlashMenuItem::Skill(skill) => {
                self.active_skill = Some(skill);
                self.focus_composer(window, cx);
                cx.notify();
            }
            SlashMenuItem::NewSkill => {
                self.open_skill_editor(None, "", "", window, cx);
            }
            SlashMenuItem::ListSkills => {
                self.refresh_skills();
                self.show_skills_panel = true;
                self.skill_editor = None;
                cx.notify();
            }
        }
    }

    fn is_any_text_input_focused(&self, window: &Window, cx: &App) -> bool {
        if self.composer.read(cx).focus_handle(cx).is_focused(window)
            || self
                .sidebar_search
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
            || self
                .projects_popout_search
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
            || self
                .new_chat_search
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
        {
            return true;
        }
        if let Some(ed) = &self.skill_editor
            && (ed.name_input.read(cx).focus_handle(cx).is_focused(window)
                || ed.desc_input.read(cx).focus_handle(cx).is_focused(window)
                || ed.body_input.read(cx).focus_handle(cx).is_focused(window))
        {
            return true;
        }
        match &self.git_panel {
            Some(GitPanel::Commit { message_input, .. }) => {
                message_input.read(cx).focus_handle(cx).is_focused(window)
            }
            Some(GitPanel::Pr {
                title_input,
                body_input,
                ..
            }) => {
                title_input.read(cx).focus_handle(cx).is_focused(window)
                    || body_input.read(cx).focus_handle(cx).is_focused(window)
            }
            None => false,
        }
    }

    fn scroll_messages_by_pages(&mut self, delta_pages: f32) {
        let bounds = self.scroll_handle.bounds();
        let step = (bounds.size.height * 0.8).max(px(160.0)) * delta_pages;
        let cur = self.scroll_handle.offset();
        let max = self.scroll_handle.max_offset();
        let next_y = (cur.y - step).clamp(-max.y, px(0.0));
        self.scroll_handle.set_offset(point(cur.x, next_y));
        self.auto_scroll = (max.y + next_y) <= px(16.0);
        self.last_offset_y = next_y;
    }

    fn scroll_messages_to_top(&mut self) {
        let cur = self.scroll_handle.offset();
        self.scroll_handle.set_offset(point(cur.x, px(0.0)));
        self.auto_scroll = self.scroll_handle.max_offset().y <= px(0.0);
        self.last_offset_y = px(0.0);
    }

    fn respond_pending_permission(&mut self, allow: bool, cx: &mut Context<Self>) -> bool {
        let found = self.blocks.iter().find_map(|b| match b {
            ChatBlock::ToolCall(tc) => {
                let pending = tc.approval.as_ref()?;
                let opt = if allow {
                    pending
                        .options
                        .iter()
                        .find(|o| o.kind == "allow_once")
                        .or_else(|| pending.options.first())
                } else {
                    pending
                        .options
                        .iter()
                        .find(|o| o.kind == "reject_once" || o.kind == "reject_always")
                        .or_else(|| pending.options.last())
                }?;
                Some((tc.id, pending.request_id.clone(), opt.clone()))
            }
            _ => None,
        });
        if let Some((block_id, req_id, opt)) = found {
            self.answer_permission(block_id, req_id, opt, cx);
            true
        } else {
            false
        }
    }

    fn handle_capture_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        let mods = &event.keystroke.modifiers;

        // -2. New Chat project-picker modal navigation.
        if self.show_new_chat_modal {
            let query = self.new_chat_search.read(cx).value().to_string();
            let projects = self.ordered_projects(&query);
            let total = projects.len() + 1; // +1 for "+ Add new project…"
            if (mods.platform || mods.control)
                && !mods.shift
                && !mods.alt
                && let Some(digit) = key.chars().next().filter(|c| key.len() == 1 && ('1'..='9').contains(c))
            {
                let idx = (digit as usize) - ('1' as usize);
                if let Some(proj) = projects.get(idx) {
                    cx.stop_propagation();
                    self.start_new_chat_in_project(proj.id.clone(), window, cx);
                    return;
                }
            }
            if !mods.platform && !mods.control && !mods.alt {
                match key {
                    "down" if total > 0 => {
                        self.new_chat_selected = (self.new_chat_selected + 1) % total;
                        cx.stop_propagation();
                        cx.notify();
                        return;
                    }
                    "up" if total > 0 => {
                        self.new_chat_selected = if self.new_chat_selected == 0 {
                            total - 1
                        } else {
                            self.new_chat_selected - 1
                        };
                        cx.stop_propagation();
                        cx.notify();
                        return;
                    }
                    "enter" => {
                        cx.stop_propagation();
                        self.confirm_new_chat_selection(window, cx);
                        return;
                    }
                    "escape" => {
                        cx.stop_propagation();
                        self.close_new_chat_modal(window, cx);
                        return;
                    }
                    _ => {}
                }
            }
        }

        // -1. Projects filter popout navigation.
        if self.show_projects_popout {
            let query = self.projects_popout_search.read(cx).value().to_string();
            let projects = self.ordered_projects(&query);
            let total = projects.len() + 2; // 0: + Add project, 1: All projects, 2..: projects
            if !mods.platform && !mods.control && !mods.alt {
                match key {
                    "down" if total > 0 => {
                        self.projects_popout_selected =
                            (self.projects_popout_selected + 1) % total;
                        cx.stop_propagation();
                        cx.notify();
                        return;
                    }
                    "up" if total > 0 => {
                        self.projects_popout_selected = if self.projects_popout_selected == 0 {
                            total - 1
                        } else {
                            self.projects_popout_selected - 1
                        };
                        cx.stop_propagation();
                        cx.notify();
                        return;
                    }
                    "enter" => {
                        cx.stop_propagation();
                        self.confirm_projects_popout_selection(window, cx);
                        return;
                    }
                    "escape" => {
                        cx.stop_propagation();
                        self.close_projects_popout(window, cx);
                        return;
                    }
                    _ => {}
                }
            }
        }

        // 0. Arrow-down from sidebar search moves focus directly to the first matching chat card.
        if key == "down"
            && !mods.platform
            && !mods.control
            && !mods.alt
            && self
                .sidebar_search
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
        {
            self.focus_edge_session_row(true, window, cx);
            cx.stop_propagation();
            return;
        }

        // 1. Slash command autocomplete navigation while menu is open.
        if let Some(query) = self.active_slash_query(cx) {
            let items = self.slash_menu_items(&query);
            if !mods.platform && !mods.control && !mods.alt {
                match key {
                    "down" if !items.is_empty() => {
                        self.slash_selected = (self.slash_selected + 1) % items.len();
                        cx.stop_propagation();
                        cx.notify();
                        return;
                    }
                    "up" if !items.is_empty() => {
                        self.slash_selected = if self.slash_selected == 0 {
                            items.len() - 1
                        } else {
                            self.slash_selected - 1
                        };
                        cx.stop_propagation();
                        cx.notify();
                        return;
                    }
                    "tab" | "enter" if !mods.shift && !items.is_empty() => {
                        let idx = self.slash_selected.min(items.len() - 1);
                        let chosen = items.into_iter().nth(idx).unwrap();
                        cx.stop_propagation();
                        self.activate_slash_item(chosen, window, cx);
                        return;
                    }
                    "escape" => {
                        self.show_slash_menu = false;
                        self.slash_dismissed_query = Some(query);
                        cx.stop_propagation();
                        cx.notify();
                        return;
                    }
                    _ => {}
                }
            }
        }

        // 2. Cmd+Enter / Ctrl+Enter panel & confirmation submission shortcuts.
        if (mods.platform || mods.control) && key == "enter" {
            if self.skill_editor.is_some() {
                cx.stop_propagation();
                self.save_skill_editor(window, cx);
                return;
            }
            if matches!(self.git_panel, Some(GitPanel::Commit { .. })) {
                let can_push = self
                    .git_status
                    .as_ref()
                    .is_some_and(|s| s.has_remote && !s.detached);
                cx.stop_propagation();
                self.submit_commit(can_push, cx);
                return;
            }
            if matches!(self.git_panel, Some(GitPanel::Pr { .. })) {
                cx.stop_propagation();
                self.submit_pr(cx);
                return;
            }
            if let Some(pending) = self.pending_send.take() {
                cx.stop_propagation();
                self.composer.update(cx, |s, cx| s.set_value("", window, cx));
                self.pending_images.clear();
                self.attach_notice = None;
                self.dispatch_send(
                    pending.display_text,
                    pending.prompt_text,
                    pending.images,
                    pending.model_id,
                    pending.unload_ids,
                    window,
                    cx,
                );
                return;
            }
            if self.skill_editor.is_none()
                && self.git_panel.is_none()
                && self.composer.read(cx).value().trim().is_empty()
                && self.respond_pending_permission(true, cx)
            {
                cx.stop_propagation();
                return;
            }
        }

        // 3. Cmd+Backspace denies a pending permission prompt when composer is empty.
        if (mods.platform || mods.control)
            && key == "backspace"
            && self.skill_editor.is_none()
            && self.git_panel.is_none()
            && self.composer.read(cx).value().trim().is_empty()
            && self.respond_pending_permission(false, cx)
        {
            cx.stop_propagation();
        }
    }

    fn handle_dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.show_new_chat_modal {
            self.close_new_chat_modal(window, cx);
        } else if self.show_projects_popout {
            self.close_projects_popout(window, cx);
        } else if self.show_shortcuts_help {
            self.show_shortcuts_help = false;
            self.focus_composer(window, cx);
            cx.notify();
        } else if self
            .sidebar_search
            .read(cx)
            .focus_handle(cx)
            .is_focused(window)
            || !self.sidebar_search.read(cx).value().trim().is_empty()
        {
            self.sidebar_search
                .update(cx, |s, cx| s.set_value("", window, cx));
            self.focus_composer(window, cx);
            cx.notify();
        } else if self.skill_editor.is_some() {
            self.skill_editor = None;
            self.skill_notice = None;
            self.focus_composer(window, cx);
            cx.notify();
        } else if self.show_skills_panel {
            self.show_skills_panel = false;
            self.focus_composer(window, cx);
            cx.notify();
        } else if self.git_panel.is_some() {
            self.git_panel = None;
            self.focus_composer(window, cx);
            cx.notify();
        } else if self.inspected_subagent.is_some() {
            self.inspected_subagent = None;
            self.focus_composer(window, cx);
            cx.notify();
        } else if self.confirm_delete_chat.take().is_some() || self.pending_send.take().is_some() {
            self.focus_composer(window, cx);
            cx.notify();
        } else if self.attach_notice.take().is_some() {
            cx.notify();
        } else if self.active_skill.is_some() && self.composer.read(cx).value().trim().is_empty() {
            self.active_skill = None;
            cx.notify();
        } else if self.busy && self.composer.read(cx).value().trim().is_empty() {
            self.stop_current(cx);
        }
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        let mods = &event.keystroke.modifiers;

        // Scroll message stream via PageUp/PageDown (and Home/End when not editing text).
        let text_focused = self.is_any_text_input_focused(window, cx);
        if !text_focused
            && (mods.platform || mods.control)
            && !mods.alt
            && key == "v"
            && let Some(item) = cx.read_from_clipboard()
            && self.attach_clipboard_item(&item, window, cx)
        {
            cx.stop_propagation();
            return;
        }
        if !mods.platform && !mods.control {
            match key {
                "pageup" if !text_focused || mods.alt => {
                    self.scroll_messages_by_pages(-1.0);
                    cx.stop_propagation();
                    cx.notify();
                }
                "pagedown" if !text_focused || mods.alt => {
                    self.scroll_messages_by_pages(1.0);
                    cx.stop_propagation();
                    cx.notify();
                }
                "home" if !text_focused && !mods.alt => {
                    self.scroll_messages_to_top();
                    cx.stop_propagation();
                    cx.notify();
                }
                "end" if !text_focused && !mods.alt => {
                    self.jump_to_bottom();
                    cx.stop_propagation();
                    cx.notify();
                }
                _ => {}
            }
        }
    }

    fn ordered_projects(&self, query: &str) -> Vec<superviser_core::project::Project> {
        let mut projects = self.backend.app.settings.lock().unwrap().projects.clone();
        if let Some(cur) = self.selected_project.as_deref()
            && let Some(pos) = projects.iter().position(|p| p.id == cur)
        {
            let current = projects.remove(pos);
            projects.insert(0, current);
        }
        let q = query.trim().to_ascii_lowercase();
        if q.is_empty() {
            return projects;
        }
        projects
            .into_iter()
            .filter(|p| {
                p.name.to_ascii_lowercase().contains(&q)
                    || p.main().display().to_string().to_ascii_lowercase().contains(&q)
            })
            .collect()
    }

    fn open_projects_popout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_new_chat_modal = false;
        self.show_projects_popout = true;
        self.projects_popout_selected = 1; // highlight "All projects" by default
        self.projects_popout_search.update(cx, |s, cx| {
            s.set_value("", window, cx);
            s.focus(window, cx);
        });
        cx.notify();
    }

    fn close_projects_popout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_projects_popout = false;
        self.focus_composer(window, cx);
        cx.notify();
    }

    fn confirm_projects_popout_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.projects_popout_search.read(cx).value().to_string();
        let projects = self.ordered_projects(&query);
        let idx = self.projects_popout_selected;
        if idx == 0 {
            self.show_projects_popout = false;
            cx.notify();
            choose_project_folder("Add a project folder", true, cx);
        } else if idx == 1 {
            self.filter_project = None;
            self.close_projects_popout(window, cx);
        } else if let Some(proj) = projects.get(idx - 2) {
            self.filter_project = Some(proj.id.clone());
            self.close_projects_popout(window, cx);
        }
    }

    fn open_new_chat_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let has_projects = !self.backend.app.settings.lock().unwrap().projects.is_empty();
        if !has_projects {
            choose_project_folder("Add a project folder", true, cx);
            return;
        }
        self.show_projects_popout = false;
        self.show_new_chat_modal = true;
        self.new_chat_selected = 0;
        self.new_chat_search.update(cx, |s, cx| {
            s.set_value("", window, cx);
            s.focus(window, cx);
        });
        cx.notify();
    }

    fn close_new_chat_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_new_chat_modal = false;
        self.focus_composer(window, cx);
        cx.notify();
    }

    fn confirm_new_chat_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.new_chat_search.read(cx).value().to_string();
        let projects = self.ordered_projects(&query);
        if let Some(proj) = projects.get(self.new_chat_selected) {
            self.start_new_chat_in_project(proj.id.clone(), window, cx);
        } else {
            self.show_new_chat_modal = false;
            cx.notify();
            choose_project_folder("Add a project folder", true, cx);
        }
    }

    fn start_new_chat_in_project(
        &mut self,
        project_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_new_chat_modal = false;
        if self.selected_project.as_deref() != Some(project_id.as_str()) {
            self.select_project(project_id, window, cx);
        } else {
            self.show_chat(None, window, cx);
        }
        self.focus_composer(window, cx);
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
        if self
            .filter_project
            .as_ref()
            .is_some_and(|id| !projects.iter().any(|p| &p.id == id))
        {
            self.filter_project = None;
        }
        if self.selected_project != prev {
            self.git_panel = None;
            self.git_notice = None;
            self.git_status = None;
            self.refresh_skills();
            self.show_chat(None, window, cx);
            self.refresh_sessions(cx);
            self.refresh_git(true, cx);
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
        self.git_panel = None;
        self.git_notice = None;
        self.git_status = None;
        self.refresh_skills();
        self.show_chat(None, window, cx);
        self.refresh_sessions(cx);
        self.refresh_git(true, cx);
    }

    fn open_session(
        &mut self,
        project_id: String,
        session_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.done_sessions.remove(&session_id);
        if self.selected_project.as_deref() != Some(project_id.as_str()) {
            self.selected_project = Some(project_id);
            self.git_panel = None;
            self.git_notice = None;
            self.git_status = None;
            self.refresh_skills();
            self.refresh_git(true, cx);
        }
        self.show_chat(Some(session_id), window, cx);
    }

    fn refresh_sessions(&mut self, cx: &mut Context<Self>) {
        let projects = self.backend.app.settings.lock().unwrap().projects.clone();
        if projects.is_empty() {
            self.sessions.clear();
            self.archived.clear();
            self.starting_note = None;
            self.sessions_error = None;
            cx.notify();
            return;
        }
        self.list_gen += 1;
        let mine = self.list_gen;
        let active_project = self.selected_project.clone();
        if active_project
            .as_deref()
            .and_then(|pid| projects.iter().find(|p| p.id == pid))
            .is_some_and(|p| p.isolation == Isolation::LinuxVm)
            && self.sessions.is_empty()
        {
            self.starting_note = Some("Starting the Linux VM…".into());
            cx.notify();
        }

        let app = self.backend.app.clone();
        let engine = self.engine;
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.backend.tokio.spawn(async move {
            let mut archived: HashSet<(String, String)> = HashSet::new();
            for proj in &projects {
                for cid in app.archived_chats(&proj.id) {
                    archived.insert((proj.id.clone(), cid));
                }
            }
            let installed_names: HashMap<String, String> = app
                .installed()
                .into_iter()
                .map(|m| (m.id, m.model.name))
                .collect();
            let mut sessions: Vec<SessionMeta> = Vec::new();
            let mut first_error: Option<String> = None;

            match engine {
                ChatEngine::OpenCode => {
                    let per_proj = app
                        .engines
                        .sessions_for_projects(&app, &projects, active_project.as_deref())
                        .await;
                    for (pid, res) in per_proj {
                        let Some(proj) = projects.iter().find(|p| p.id == pid) else {
                            continue;
                        };
                        match res {
                            Ok(raw) => {
                                for s in raw.as_array().into_iter().flatten() {
                                    let Some(id) = s["sessionId"].as_str().map(str::to_string)
                                    else {
                                        continue;
                                    };
                                    let raw_title = s["title"].as_str().unwrap_or("");
                                    let title = if raw_title.starts_with("New session - ")
                                        || raw_title.is_empty()
                                    {
                                        "New chat".to_string()
                                    } else {
                                        raw_title.to_string()
                                    };
                                    let updated_at = parse_session_timestamp(s);
                                    let model_name = app.chat_model(&pid, &id).and_then(|mid| {
                                        installed_names.get(&mid).cloned()
                                    });
                                    sessions.push(SessionMeta {
                                        id,
                                        project_id: pid.clone(),
                                        project_name: proj.name.clone(),
                                        isolation: proj.isolation,
                                        title,
                                        updated_at,
                                        model_name,
                                    });
                                }
                            }
                            Err(e) => {
                                if active_project.as_deref() == Some(pid.as_str())
                                    || first_error.is_none()
                                {
                                    first_error = Some(e);
                                }
                            }
                        }
                    }
                }
                ChatEngine::Builtin => {
                    for proj in &projects {
                        for c in Chat::list_in(&app.paths.data, &proj.id) {
                            let model_name = app
                                .chat_model(&proj.id, &c.id)
                                .and_then(|mid| installed_names.get(&mid).cloned());
                            sessions.push(SessionMeta {
                                id: c.id,
                                project_id: proj.id.clone(),
                                project_name: proj.name.clone(),
                                isolation: proj.isolation,
                                title: c.title,
                                updated_at: (c.updated > 0).then_some(c.updated),
                                model_name,
                            });
                        }
                    }
                }
            }

            let busy_map = match engine {
                ChatEngine::OpenCode => app.engines.all_busy(),
                ChatEngine::Builtin => sessions
                    .iter()
                    .filter_map(|s| {
                        app.chats
                            .busy_since(&s.id)
                            .map(|since| (s.id.clone(), since))
                    })
                    .collect(),
            };

            // Keep busy sessions near the top, then most recently updated across all projects.
            sessions.sort_by(|a, b| {
                let a_busy = busy_map.contains_key(&a.id);
                let b_busy = busy_map.contains_key(&b.id);
                b_busy
                    .cmp(&a_busy)
                    .then_with(|| b.updated_at.unwrap_or(0).cmp(&a.updated_at.unwrap_or(0)))
            });

            let err = if sessions.is_empty() {
                first_error
            } else {
                None
            };
            let _ = tx.send((sessions, archived, busy_map, err));
        });

        cx.spawn(async move |this, cx| {
            let Ok((sessions, archived, busy_map, err)) = rx.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                if this.list_gen != mine {
                    return;
                }
                this.starting_note = None;
                this.sessions = sessions;
                this.archived = archived;
                this.sessions_error = err;
                for (sid, since) in busy_map {
                    this.working_since_by_session.entry(sid).or_insert(since);
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
        self.pending_images.clear();
        self.attach_notice = None;
        self.confirm_delete_chat = None;
        self.inspected_subagent = None;
        self.subagents = match (&self.selected_project, &session_id) {
            (Some(pid), Some(sid)) => self.backend.app.subagents.for_session(pid, sid),
            _ => Vec::new(),
        };
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
            self.focus_composer(window, cx);
            cx.notify();
            return;
        };
        self.focus_composer(window, cx);

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
                ChatEngine::Builtin => match Chat::load_in(&app.paths.data, &pid, &sid) {
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

    fn archive_chat(
        &mut self,
        project_id: String,
        chat_id: String,
        archive: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(e) = self
            .backend
            .app
            .set_chat_archived(&project_id, &chat_id, archive)
        {
            self.blocks.push(ChatBlock::StatusNote {
                text: format!("{e:#}"),
                is_error: true,
            });
            cx.notify();
            return;
        }
        let key = (project_id, chat_id.clone());
        if archive {
            self.archived.insert(key);
            if self.active_session.as_deref() == Some(chat_id.as_str()) {
                self.show_chat(None, window, cx);
            }
        } else {
            self.archived.remove(&key);
        }
        cx.notify();
    }

    fn delete_chat(
        &mut self,
        project_id: String,
        chat_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pid = project_id;
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
                    Chat::delete_in(&app.paths.data, &pid, &chat_id)
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

    fn alloc_image_id(&mut self) -> usize {
        let id = self.next_image_id;
        self.next_image_id += 1;
        id
    }

    fn attach_image_paths(
        &mut self,
        paths: &[PathBuf],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut added = 0usize;
        let mut errors: Vec<String> = Vec::new();
        for path in paths {
            if self.pending_images.len() >= MAX_IMAGES_PER_MESSAGE {
                errors.push(format!("Maximum {MAX_IMAGES_PER_MESSAGE} images per message."));
                break;
            }
            match ImageAttachment::from_path(path) {
                Ok(att) => {
                    let id = self.alloc_image_id();
                    self.pending_images
                        .push(AttachedImage::from_attachment(id, att));
                    added += 1;
                }
                Err(e) => errors.push(format!("{e:#}")),
            }
        }
        self.attach_notice = if errors.is_empty() {
            None
        } else {
            Some(errors.join(" · "))
        };
        if added > 0 {
            self.focus_composer(window, cx);
        }
        cx.notify();
    }

    fn attach_clipboard_item(
        &mut self,
        item: &ClipboardItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut attached_any = false;
        let mut errors: Vec<String> = Vec::new();

        // 1. Direct clipboard image (e.g. macOS screenshot via ⌘⌃⇧4 or Copy Image).
        for entry in item.entries() {
            if let ClipboardEntry::Image(gpui_img) = entry {
                if self.pending_images.len() >= MAX_IMAGES_PER_MESSAGE {
                    errors.push(format!("Maximum {MAX_IMAGES_PER_MESSAGE} images per message."));
                    attached_any = true;
                    break;
                }
                let id = self.alloc_image_id();
                let name = format!("Screenshot {id}.{}", gpui_img.format.extension());
                match AttachedImage::from_gpui_image(id, gpui_img, name) {
                    Ok(attached) => {
                        self.pending_images.push(attached);
                        attached_any = true;
                    }
                    Err(e) => {
                        errors.push(format!("{e:#}"));
                        attached_any = true;
                    }
                }
            }
        }

        // 2. Copied image files from Finder (⌘C on PNG/JPEG/etc., then ⌘V in chat).
        if !attached_any {
            let image_paths: Vec<PathBuf> = item
                .entries()
                .iter()
                .filter_map(|e| match e {
                    ClipboardEntry::ExternalPaths(paths) => Some(paths.paths()),
                    _ => None,
                })
                .flatten()
                .filter(|p| attachments::is_supported_image_path(p))
                .cloned()
                .collect();
            if !image_paths.is_empty() {
                self.attach_image_paths(&image_paths, window, cx);
                return true;
            }
        }

        if attached_any {
            self.attach_notice = if errors.is_empty() {
                None
            } else {
                Some(errors.join(" · "))
            };
            self.focus_composer(window, cx);
            cx.notify();
        }
        attached_any
    }

    fn choose_images(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach images".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else {
                return;
            };
            if paths.is_empty() {
                return;
            }
            let _ = this.update_in(cx, |this, window, cx| {
                this.attach_image_paths(&paths, window, cx);
            });
        })
        .detach();
    }

    fn remove_pending_image(&mut self, id: usize, cx: &mut Context<Self>) {
        self.pending_images.retain(|img| img.id != id);
        if self.pending_images.len() < MAX_IMAGES_PER_MESSAGE {
            self.attach_notice = None;
        }
        cx.notify();
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
            if text.is_empty() && self.pending_images.is_empty() {
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
                        images: self.pending_images.clone(),
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
        self.attach_notice = None;
        let images = std::mem::take(&mut self.pending_images);
        self.composer
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.dispatch_send(display_text, prompt_text, images, model_id, Vec::new(), window, cx);
    }

    #[allow(clippy::too_many_arguments)]
    fn dispatch_send(
        &mut self,
        display_text: String,
        prompt_text: String,
        images: Vec<AttachedImage>,
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
            images: images.clone(),
        });
        let raw_images: Vec<ImageAttachment> = images.iter().map(|i| i.attachment.clone()).collect();
        let started_at = Instant::now();
        self.busy = true;
        self.working_since = Some(started_at);
        self.working_phase = WorkingPhase::StartingAgent;
        if let Some(sid) = &start_session {
            self.done_sessions.remove(sid);
            self.approval_pending_sessions.remove(sid);
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
                        let include_paths = app.settings.lock().unwrap().include_image_paths;
                        let path_note = include_paths
                            .then(|| attachments::format_image_paths_note(&raw_images, Some(&proj)))
                            .flatten();
                        eng.prompt(
                            app.bus.clone(),
                            &sid,
                            &prompt_text,
                            &raw_images,
                            path_note,
                            &installed_model.alias,
                        )
                        .map_err(|e| format!("{e:#}"))?;
                        sid
                    }
                    ChatEngine::Builtin => superviser_backend::agent::send(
                        app.clone(),
                        &pid_task,
                        start_session.as_deref(),
                        &prompt_text,
                        &raw_images,
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
                        this.archived.remove(&(pid.clone(), target_session.clone()));
                        this.active_session = Some(target_session.clone());
                        this.subagents = this
                            .backend
                            .app
                            .subagents
                            .for_session(&pid, &target_session);
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
                        if this.composer.read(cx).value().trim().is_empty() && !display_text.is_empty() {
                            this.composer.update(cx, |s, cx| {
                                s.set_value(display_text.clone(), window, cx)
                            });
                        }
                        if this.pending_images.is_empty() && !images.is_empty() {
                            this.pending_images = images;
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
        let still_waiting = self
            .blocks
            .iter()
            .any(|b| matches!(b, ChatBlock::ToolCall(tc) if tc.approval.is_some()));
        if !still_waiting
            && let Some(sid) = self.active_session.as_deref()
        {
            self.approval_pending_sessions.remove(sid);
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
        if self.creating_session
            && matches!(event, AppEvent::Acp(_) | AppEvent::Chat(_) | AppEvent::Subagent(_))
        {
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
                self.refresh_sessions(cx);
                cx.notify();
            }
            AppEvent::Acp(ev) => self.handle_acp_event(ev, cx),
            AppEvent::Chat(ev) => self.handle_chat_event(ev, cx),
            AppEvent::Subagent(rec) => self.handle_subagent_event(rec, cx),
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
                self.approval_pending_sessions.insert(session.clone());
                if self.selected_project.as_deref() != Some(project.as_str())
                    || self.active_session.as_deref() != Some(session.as_str())
                {
                    cx.notify();
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
                    self.done_sessions.remove(&session);
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
                    }
                    cx.notify();
                } else {
                    self.working_since_by_session.remove(&session);
                    self.working_phase_by_session.remove(&session);
                    self.approval_pending_sessions.remove(&session);
                    if error.is_none() && stop_reason.as_deref() != Some("cancelled") {
                        self.done_sessions.insert(session.clone());
                    }
                    if self.selected_project.as_deref() == Some(project.as_str())
                        && self.active_session.as_deref() == Some(session.as_str())
                    {
                        self.finish_turn(error, stop_reason, cx);
                        cx.notify();
                    } else {
                        self.refresh_sessions(cx);
                    }
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
                self.approval_pending_sessions.insert(chat.clone());
                if self.active_session.as_deref() != Some(chat.as_str()) {
                    cx.notify();
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
            ChatEvent::Usage { chat, used, size } => {
                if self.active_session.as_deref() == Some(chat.as_str()) && size > 0 {
                    self.usage = Some((used, size));
                    cx.notify();
                }
            }
            ChatEvent::Notice { chat, text } => {
                if self.active_session.as_deref() == Some(chat.as_str()) {
                    self.blocks.push(ChatBlock::StatusNote {
                        text,
                        is_error: false,
                    });
                    self.scroll_if_following();
                    cx.notify();
                }
            }
            ChatEvent::State { chat, busy, error } => {
                if busy {
                    self.done_sessions.remove(&chat);
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
                    }
                    cx.notify();
                } else {
                    self.working_since_by_session.remove(&chat);
                    self.working_phase_by_session.remove(&chat);
                    self.approval_pending_sessions.remove(&chat);
                    if error.is_none() {
                        self.done_sessions.insert(chat.clone());
                    }
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
        self.refresh_git(false, cx);
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
                let raw_text = content_text(&update["content"]);
                let text = if raw_text.trim_start().starts_with(attachments::IMAGE_PATHS_HEADER) {
                    String::new()
                } else {
                    raw_text
                };
                let image = if update["content"]["type"].as_str() == Some("image") {
                    let mime = update["content"]["mimeType"]
                        .as_str()
                        .unwrap_or("image/png");
                    let data = update["content"]["data"].as_str().unwrap_or("");
                    if !data.is_empty() {
                        let id = self.alloc_image_id();
                        let att = ImageAttachment {
                            name: format!("Image {id}"),
                            mime_type: attachments::normalize_mime(mime)
                                .unwrap_or("image/png")
                                .to_string(),
                            data_base64: data.to_string(),
                            size_bytes: (data.len() * 3) / 4,
                            source_path: None,
                        };
                        Some(AttachedImage::from_attachment(id, att))
                    } else {
                        None
                    }
                } else {
                    None
                };
                if text.is_empty() && image.is_none() {
                    return;
                }
                self.seal_live_blocks();
                let msg_id = update["messageId"].as_str().map(str::to_string);
                if let Some(ref mid) = msg_id
                    && let Some(ChatBlock::User {
                        text: existing,
                        images: existing_images,
                        ..
                    }) = self.blocks.iter_mut().find(
                        |b| matches!(b, ChatBlock::User { key: Some(k), .. } if k == mid),
                    )
                {
                    existing.push_str(&text);
                    if let Some(img_att) = image {
                        existing_images.push(img_att);
                    }
                    return;
                }
                let mut images = Vec::new();
                if let Some(img_att) = image {
                    images.push(img_att);
                }
                self.blocks.push(ChatBlock::User {
                    key: msg_id,
                    text,
                    images,
                });
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
                    let (text, images) = match &message["content"] {
                        Value::String(s) => (s.clone(), Vec::new()),
                        Value::Array(parts) => {
                            let mut text_parts = Vec::new();
                            let mut images = Vec::new();
                            for part in parts {
                                match part["type"].as_str() {
                                    Some("text" | "input_text") => {
                                        let is_meta = part
                                            .get("superviser_meta")
                                            .and_then(Value::as_bool)
                                            .unwrap_or(false);
                                        if !is_meta
                                            && let Some(t) = part["text"].as_str()
                                        {
                                            text_parts.push(t);
                                        }
                                    }
                                    Some("image_url") => {
                                        let url = part["image_url"]["url"]
                                            .as_str()
                                            .or_else(|| part["image_url"].as_str());
                                        if let Some(url) = url {
                                            let id = self.alloc_image_id();
                                            if let Some(att) = ImageAttachment::from_data_url(
                                                format!("Image {id}"),
                                                url,
                                            ) {
                                                images.push(AttachedImage::from_attachment(
                                                    id, att,
                                                ));
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            (text_parts.join("\n"), images)
                        }
                        _ => (String::new(), Vec::new()),
                    };
                    self.blocks.push(ChatBlock::User {
                        key: replay_id,
                        text,
                        images,
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

        let desc_for_name = desc_input.clone();
        let name_sub = cx.subscribe_in(
            &name_input,
            window,
            move |_this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    desc_for_name.update(cx, |s, cx| s.focus(window, cx));
                }
            },
        );
        let body_for_desc = body_input.clone();
        let desc_sub = cx.subscribe_in(
            &desc_input,
            window,
            move |_this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    body_for_desc.update(cx, |s, cx| s.focus(window, cx));
                }
            },
        );

        name_input.update(cx, |s, cx| s.focus(window, cx));

        self.show_skills_panel = false;
        self.show_slash_menu = false;
        self.skill_notice = None;
        self.skill_editor = Some(SkillEditor {
            scope,
            name_input,
            desc_input,
            body_input,
            editing_original: existing,
            _subs: vec![name_sub, desc_sub],
        });
        cx.notify();
    }

    fn save_skill_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
                self.focus_composer(window, cx);
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
        let items = self.slash_menu_items(query);
        let selected_idx = if items.is_empty() {
            0
        } else {
            self.slash_selected.min(items.len() - 1)
        };
        let query_owned = query.to_string();

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
                            .child("Slash commands & skills  ·  ↑↓ navigate  ·  Enter/Tab select  ·  Esc close"),
                    )
                    .child(
                        Button::new("slash-menu-close")
                            .ghost()
                            .xsmall()
                            .label("×")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.show_slash_menu = false;
                                this.slash_dismissed_query = Some(query_owned.clone());
                                this.focus_composer(window, cx);
                                cx.notify();
                            })),
                    ),
            )
            .children(items.into_iter().enumerate().map(|(i, item)| {
                let is_selected = i == selected_idx;
                match item {
                    SlashMenuItem::Skill(skill) => {
                        let name = skill.name.clone();
                        let desc = skill.description.clone();
                        let scope_label = skill.scope.label();
                        h_flex()
                            .id(("slash-item", i))
                            .cursor_pointer()
                            .px_2()
                            .py(px(5.0))
                            .gap_2()
                            .items_center()
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(if is_selected { p.accent } else { p.panel })
                            .when(is_selected, move |d| d.bg(p.chip))
                            .hover(move |d| d.bg(p.chip))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.activate_slash_item(
                                    SlashMenuItem::Skill(skill.clone()),
                                    window,
                                    cx,
                                );
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
                            .into_any_element()
                    }
                    SlashMenuItem::NewSkill => h_flex()
                        .id(("slash-item", i))
                        .cursor_pointer()
                        .px_2()
                        .py(px(5.0))
                        .gap_2()
                        .items_center()
                        .rounded(px(6.0))
                        .border_1()
                        .border_color(if is_selected { p.accent } else { p.panel })
                        .when(is_selected, move |d| d.bg(p.chip))
                        .hover(move |d| d.bg(p.chip))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.activate_slash_item(SlashMenuItem::NewSkill, window, cx);
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
                        )
                        .into_any_element(),
                    SlashMenuItem::ListSkills => h_flex()
                        .id(("slash-item", i))
                        .cursor_pointer()
                        .px_2()
                        .py(px(5.0))
                        .gap_2()
                        .items_center()
                        .rounded(px(6.0))
                        .border_1()
                        .border_color(if is_selected { p.accent } else { p.panel })
                        .when(is_selected, move |d| d.bg(p.chip))
                        .hover(move |d| d.bg(p.chip))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.activate_slash_item(SlashMenuItem::ListSkills, window, cx);
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
                                .child(format!(
                                    "Browse & manage saved skills ({})",
                                    self.skills.len()
                                )),
                        )
                        .into_any_element(),
                }
            }))
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
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.show_skills_panel = false;
                                        this.focus_composer(window, cx);
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
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.skill_editor = None;
                                        this.skill_notice = None;
                                        this.focus_composer(window, cx);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("skill-save")
                                    .primary()
                                    .small()
                                    .label("Save skill")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.save_skill_editor(window, cx);
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
                                        state.focus(window, cx);
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

    fn render_block(
        &self,
        block: &ChatBlock,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_dark = cx.theme().mode == ThemeMode::Dark;
        let p = palette(is_dark);

        match block {
            ChatBlock::User { text, images, .. } => h_flex().w_full().flex_shrink_0().justify_end().child(
                v_flex()
                    .max_w(px(560.0))
                    .px_3()
                    .py_2()
                    .gap_2()
                    .rounded(px(10.0))
                    .bg(p.accent)
                    .text_color(p.accent_text)
                    .text_sm()
                    .when(!images.is_empty(), |bubble| {
                        bubble.child(
                            h_flex()
                                .flex_wrap()
                                .gap_2()
                                .children(images.iter().map(|attached| {
                                    v_flex()
                                        .gap_1()
                                        .max_w(px(220.0))
                                        .when_some(attached.preview.clone(), |card, preview| {
                                            card.child(
                                                img(preview)
                                                    .max_w(px(220.0))
                                                    .max_h(px(160.0))
                                                    .object_fit(ObjectFit::Contain)
                                                    .rounded(px(6.0))
                                                    .border_1()
                                                    .border_color(p.accent_text.opacity(0.25)),
                                            )
                                        })
                                        .child(
                                            div()
                                                .text_size(px(10.0))
                                                .text_color(p.accent_text.opacity(0.85))
                                                .truncate()
                                                .child(attached.attachment.name.clone()),
                                        )
                                })),
                        )
                    })
                    .when(!text.is_empty(), |bubble| bubble.child(text.clone())),
            ),
            ChatBlock::Thinking { id, text, open, .. } => {
                let block_id = *id;
                let arrow = if *open { "▾" } else { "▸" };
                let focus_handle = window
                    .use_keyed_state(("thinking-focus", block_id), cx, |_, cx| cx.focus_handle())
                    .read(cx)
                    .clone();
                let is_focused = focus_handle.is_focused(window);
                v_flex()
                    .w_full()
                    .flex_shrink_0()
                    .gap_1()
                    .child(
                        div()
                            .id(("thinking-toggle", block_id))
                            .track_focus(&focus_handle)
                            .tab_stop(true)
                            .tab_index(0)
                            .cursor_pointer()
                            .px_1()
                            .rounded(px(4.0))
                            .border_1()
                            .border_color(if is_focused { p.accent } else { p.bg })
                            .text_xs()
                            .text_color(p.muted)
                            .child(format!("{arrow} Thinking"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.toggle_block_open(block_id, cx);
                            }))
                            .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, _, cx| {
                                let mods = &ev.keystroke.modifiers;
                                if !mods.platform
                                    && !mods.control
                                    && !mods.alt
                                    && matches!(ev.keystroke.key.as_str(), "enter" | "space")
                                {
                                    cx.stop_propagation();
                                    this.toggle_block_open(block_id, cx);
                                }
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
                if let Some(sub) = self.subagents.iter().find(|s| s.id == tc.call_id).cloned() {
                    return self.render_inline_subagent_card(tc.id, &sub, p, cx);
                }
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
                let tool_focus = window
                    .use_keyed_state(("tool-focus", block_id), cx, |_, cx| cx.focus_handle())
                    .read(cx)
                    .clone();
                let tool_focused = tool_focus.is_focused(window);
                let header = if is_execute {
                    h_flex()
                        .px_3()
                        .py_2()
                        .gap_2()
                        .items_center()
                        .child(
                            h_flex()
                                .id(("tool-head-left", block_id))
                                .track_focus(&tool_focus)
                                .tab_stop(true)
                                .tab_index(0)
                                .flex_shrink_0()
                                .cursor_pointer()
                                .px_1()
                                .rounded(px(4.0))
                                .border_1()
                                .border_color(if tool_focused { p.accent } else { p.panel })
                                .gap_2()
                                .items_center()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.toggle_block_open(block_id, cx);
                                }))
                                .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, _, cx| {
                                    let mods = &ev.keystroke.modifiers;
                                    if !mods.platform
                                        && !mods.control
                                        && !mods.alt
                                        && matches!(ev.keystroke.key.as_str(), "enter" | "space")
                                    {
                                        cx.stop_propagation();
                                        this.toggle_block_open(block_id, cx);
                                    }
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
                        .track_focus(&tool_focus)
                        .tab_stop(true)
                        .tab_index(0)
                        .cursor_pointer()
                        .px_3()
                        .py_2()
                        .gap_2()
                        .items_center()
                        .rounded(px(7.0))
                        .border_1()
                        .border_color(if tool_focused { p.accent } else { p.panel })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_block_open(block_id, cx);
                        }))
                        .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, _, cx| {
                            let mods = &ev.keystroke.modifiers;
                            if !mods.platform
                                && !mods.control
                                && !mods.alt
                                && matches!(ev.keystroke.key.as_str(), "enter" | "space")
                            {
                                cx.stop_propagation();
                                this.toggle_block_open(block_id, cx);
                            }
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_dark = cx.theme().mode == ThemeMode::Dark;
        let pid_open = s.project_id.clone();
        let pid_key = s.project_id.clone();
        let pid_arch = s.project_id.clone();
        let pid_del = s.project_id.clone();
        let sid_open = s.id.clone();
        let sid_key = s.id.clone();
        let sid_arch = s.id.clone();
        let sid_del = s.id.clone();
        let title_del = s.title.clone();
        let active = self.active_session.as_deref() == Some(s.id.as_str())
            && self.selected_project.as_deref() == Some(s.project_id.as_str());
        let focus_handle = self
            .session_focus_handles
            .get(&s.id)
            .cloned()
            .unwrap_or_else(|| {
                window
                    .use_keyed_state(("session-focus", idx), cx, |_, cx| cx.focus_handle())
                    .read(cx)
                    .clone()
            });
        let is_focused = focus_handle.is_focused(window);
        let awaiting_approval = self.approval_pending_sessions.contains(&s.id);
        let working_elapsed = self
            .working_since_by_session
            .get(&s.id)
            .map(|since| format_working_compact(since.elapsed().as_secs()));
        let is_done = self.done_sessions.contains(&s.id);
        let rel_time = s.updated_at.map(format_relative_time);
        let monogram = project_monogram(&s.project_name);
        let (badge_bg, badge_fg) = project_badge_colors(&s.project_id, is_dark);
        let env_label = match s.isolation {
            Isolation::MacosSandbox => "macOS sandbox",
            Isolation::LinuxVm => "Linux VM",
        };
        let footer_meta = match &s.model_name {
            Some(model) => format!("{env_label} · {model}"),
            None => env_label.to_string(),
        };

        v_flex()
            .id(("session-row", idx))
            .track_focus(&focus_handle)
            .tab_stop(true)
            .tab_index(0)
            .flex_shrink_0()
            .w_full()
            .cursor_pointer()
            .px_2p5()
            .py_2()
            .gap_1()
            .rounded(px(8.0))
            .border_1()
            .border_color(if is_focused {
                p.accent
            } else if active {
                p.line
            } else {
                p.panel
            })
            .when(active || is_focused, |d| d.bg(p.chip))
            .hover(|d| d.bg(p.chip.opacity(0.75)))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_session(pid_open.clone(), sid_open.clone(), window, cx);
            }))
            .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, window, cx| {
                let mods = &ev.keystroke.modifiers;
                if mods.platform || mods.control || mods.alt {
                    return;
                }
                match ev.keystroke.key.as_str() {
                    "enter" | "space" => {
                        cx.stop_propagation();
                        this.open_session(pid_key.clone(), sid_key.clone(), window, cx);
                    }
                    "up" => {
                        cx.stop_propagation();
                        this.focus_adjacent_session_row(&sid_key, -1, window, cx);
                    }
                    "down" => {
                        cx.stop_propagation();
                        this.focus_adjacent_session_row(&sid_key, 1, window, cx);
                    }
                    "home" => {
                        cx.stop_propagation();
                        this.focus_edge_session_row(true, window, cx);
                    }
                    "end" => {
                        cx.stop_propagation();
                        this.focus_edge_session_row(false, window, cx);
                    }
                    _ => {}
                }
            }))
            // Row 1: Project monogram + project name on left, live Working / relative time on right
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .items_center()
                            .gap_1p5()
                            .child(
                                h_flex()
                                    .size(px(18.0))
                                    .flex_shrink_0()
                                    .rounded(px(4.0))
                                    .items_center()
                                    .justify_center()
                                    .bg(badge_bg)
                                    .text_color(badge_fg)
                                    .text_size(px(9.0))
                                    .font_weight(gpui_kit::FontWeight::BOLD)
                                    .child(monogram),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_xs()
                                    .text_color(p.muted)
                                    .truncate()
                                    .child(s.project_name.clone()),
                            ),
                    )
                    .when(awaiting_approval, |row| {
                        row.child(
                            h_flex()
                                .flex_shrink_0()
                                .items_center()
                                .gap_1()
                                .child(
                                    div()
                                        .text_xs()
                                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                        .text_color(p.warn)
                                        .child("⚠ Approval"),
                                ),
                        )
                    })
                    .when(!awaiting_approval, |row| {
                        row.when_some(working_elapsed, |row, elapsed| {
                            row.child(
                                h_flex()
                                    .flex_shrink_0()
                                    .items_center()
                                    .gap_1()
                                    .child(div().size(px(6.0)).rounded_full().bg(p.accent))
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                                            .text_color(p.accent)
                                            .child(format!("Working {elapsed}")),
                                    ),
                            )
                        })
                        .when(
                            !self.working_since_by_session.contains_key(&s.id) && is_done,
                            |row| {
                                row.child(
                                    div()
                                        .flex_shrink_0()
                                        .text_xs()
                                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                                        .text_color(p.good)
                                        .child("✓ Done"),
                                )
                            },
                        )
                        .when(
                            !self.working_since_by_session.contains_key(&s.id) && !is_done,
                            |row| {
                                row.when_some(rel_time, |row, t| {
                                    row.child(
                                        div()
                                            .flex_shrink_0()
                                            .text_xs()
                                            .text_color(p.muted)
                                            .child(t),
                                    )
                                })
                            },
                        )
                    }),
            )
            // Row 2: Multi-line auto-named chat title
            .child(
                div()
                    .w_full()
                    .text_sm()
                    .line_height(px(18.0))
                    .max_h(px(38.0))
                    .overflow_hidden()
                    .text_color(if active { p.text } else { p.text.opacity(0.92) })
                    .font_weight(if active {
                        gpui_kit::FontWeight::SEMIBOLD
                    } else {
                        gpui_kit::FontWeight::MEDIUM
                    })
                    .child(s.title.clone()),
            )
            // Row 3: Environment / model metadata + Archive / Delete actions
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .gap_1()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(p.muted)
                            .truncate()
                            .child(footer_meta),
                    )
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .gap_0p5()
                            .items_center()
                            .child(
                                Button::new(("session-arch", idx))
                                    .ghost()
                                    .xsmall()
                                    .icon(if is_archived {
                                        IconName::ArchiveRestore
                                    } else {
                                        IconName::Archive
                                    })
                                    .tooltip(if is_archived {
                                        "Restore chat to active list"
                                    } else {
                                        "Archive chat"
                                    })
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.archive_chat(
                                            pid_arch.clone(),
                                            sid_arch.clone(),
                                            !is_archived,
                                            window,
                                            cx,
                                        );
                                    })),
                            )
                            .child(
                                Button::new(("session-del", idx))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Trash)
                                    .tooltip("Delete chat")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.confirm_delete_chat = Some((
                                            pid_del.clone(),
                                            sid_del.clone(),
                                            title_del.clone(),
                                        ));
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
    }
    // --- First-class Sub-Agent tracking UI (T3 Code-style strip + inspector drawer) ---

    fn handle_subagent_event(&mut self, rec: SubagentRecord, cx: &mut Context<Self>) {
        if self.selected_project.as_deref() != Some(rec.project.as_str())
            || self.active_session.as_deref() != Some(rec.parent_session.as_str())
        {
            return;
        }
        if let Some(existing) = self.subagents.iter_mut().find(|s| s.id == rec.id) {
            *existing = rec;
        } else {
            self.subagents.push(rec);
        }
        cx.notify();
    }

    fn toggle_inspect_subagent(&mut self, id: String, cx: &mut Context<Self>) {
        if self.inspected_subagent.as_deref() == Some(id.as_str()) {
            self.inspected_subagent = None;
        } else {
            self.inspected_subagent = Some(id);
        }
        cx.notify();
    }

    fn cancel_subagent(&mut self, id: String, cx: &mut Context<Self>) {
        let app = self.backend.app.clone();
        app.subagents.cancel(&id, &app.bus);
        cx.notify();
    }

    fn subagent_elapsed(rec: &SubagentRecord) -> String {
        let end = rec.finished_ms.unwrap_or_else(subagents::now_ms);
        let secs = end.saturating_sub(rec.started_ms) / 1000;
        format_elapsed(secs)
    }

    fn subagent_status_visual(
        state: SubagentState,
        p: crate::theme::Palette,
    ) -> (&'static str, Hsla) {
        match state {
            SubagentState::Running => ("●", p.accent),
            SubagentState::Completed => ("✓", p.good),
            SubagentState::Failed => ("✕", p.bad),
            SubagentState::Cancelled => ("○", p.muted),
        }
    }

    fn render_inline_subagent_card(
        &self,
        block_id: usize,
        rec: &SubagentRecord,
        p: crate::theme::Palette,
        cx: &mut Context<Self>,
    ) -> Div {
        let (glyph, color) = Self::subagent_status_visual(rec.state, p);
        let elapsed = Self::subagent_elapsed(rec);
        let inspected = self.inspected_subagent.as_deref() == Some(rec.id.as_str());
        let step_count = rec.steps.len();
        let latest_step = rec.steps.last().map(|s| {
            if s.subject.is_empty() {
                s.verb.clone()
            } else {
                format!("{} {}", s.verb, s.subject)
            }
        });
        let sub_id_inspect = rec.id.clone();
        let sub_id_stop = rec.id.clone();

        v_flex()
            .w_full()
            .flex_shrink_0()
            .rounded(px(8.0))
            .border_1()
            .border_color(if inspected { p.accent } else { p.line })
            .bg(p.panel)
            .px_3()
            .py_2()
            .gap_1()
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .items_center()
                            .gap_2()
                            .child(div().text_xs().text_color(color).child(glyph))
                            .child(
                                Tag::secondary()
                                    .small()
                                    .child(format!("Sub-agent · {}", rec.role)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_sm()
                                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                                    .truncate()
                                    .child(rec.title.clone()),
                            ),
                    )
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .items_center()
                            .gap_2()
                            .when_some(rec.model.clone(), |row, m| {
                                row.child(div().text_xs().text_color(p.muted).child(m))
                            })
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(color)
                                    .child(format!("{} · {elapsed}", rec.state.label())),
                            )
                            .when(rec.state.is_active() && rec.cancellable, |row| {
                                row.child(
                                    Button::new(("inline-sub-stop", block_id))
                                        .xsmall()
                                        .danger()
                                        .label("Stop")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.cancel_subagent(sub_id_stop.clone(), cx);
                                        })),
                                )
                            })
                            .child(
                                Button::new(("inline-sub-inspect", block_id))
                                    .xsmall()
                                    .when(inspected, |b| b.primary())
                                    .label(if inspected { "Inspecting" } else { "Inspect" })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_inspect_subagent(sub_id_inspect.clone(), cx);
                                    })),
                            ),
                    ),
            )
            .when(step_count > 0 || rec.error.is_some(), |col| {
                let note = if let Some(err) = &rec.error {
                    err.lines().next().unwrap_or(err).to_string()
                } else if let Some(last) = latest_step {
                    format!(
                        "{step_count} step{} · {last}",
                        if step_count == 1 { "" } else { "s" }
                    )
                } else {
                    format!(
                        "{step_count} step{}",
                        if step_count == 1 { "" } else { "s" }
                    )
                };
                col.child(
                    div()
                        .text_xs()
                        .text_color(if rec.error.is_some() { p.bad } else { p.muted })
                        .truncate()
                        .child(note),
                )
            })
    }

    fn render_subagent_strip(
        &self,
        p: crate::theme::Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let running = self.subagents.iter().filter(|s| s.state.is_active()).count();
        let total = self.subagents.len();
        let heading = if running > 0 {
            format!("Sub-agents ({running}/{total} active)")
        } else {
            format!("Sub-agents ({total})")
        };

        h_flex()
            .flex_shrink_0()
            .px_4()
            .py(px(5.0))
            .gap_2()
            .items_center()
            .flex_wrap()
            .bg(p.panel)
            .border_b_1()
            .border_color(p.line)
            .child(
                div()
                    .text_xs()
                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                    .text_color(p.muted)
                    .child(heading),
            )
            .children(self.subagents.iter().enumerate().map(|(i, rec)| {
                let (glyph, color) = Self::subagent_status_visual(rec.state, p);
                let elapsed = Self::subagent_elapsed(rec);
                let active = self.inspected_subagent.as_deref() == Some(rec.id.as_str());
                let id_click = rec.id.clone();
                let id_key = rec.id.clone();
                let pill_focus = window
                    .use_keyed_state(("subagent-pill-focus", i), cx, |_, cx| cx.focus_handle())
                    .read(cx)
                    .clone();
                let is_focused = pill_focus.is_focused(window);
                let steps_label = if rec.steps.is_empty() {
                    String::new()
                } else {
                    format!(" · {} steps", rec.steps.len())
                };
                h_flex()
                    .id(("subagent-pill", i))
                    .track_focus(&pill_focus)
                    .tab_stop(true)
                    .tab_index(0)
                    .cursor_pointer()
                    .px_2()
                    .py(px(2.0))
                    .gap_1()
                    .items_center()
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(if active || is_focused { p.accent } else { p.line })
                    .bg(if active { p.accent.opacity(0.14) } else { p.chip })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_inspect_subagent(id_click.clone(), cx);
                    }))
                    .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, _, cx| {
                        let mods = &ev.keystroke.modifiers;
                        if !mods.platform
                            && !mods.control
                            && !mods.alt
                            && matches!(ev.keystroke.key.as_str(), "enter" | "space")
                        {
                            cx.stop_propagation();
                            this.toggle_inspect_subagent(id_key.clone(), cx);
                        }
                    }))
                    .child(div().text_xs().text_color(color).child(glyph))
                    .child(
                        div()
                            .text_xs()
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .child(rec.role.clone()),
                    )
                    .child(
                        div()
                            .max_w(px(180.0))
                            .text_xs()
                            .text_color(p.muted)
                            .truncate()
                            .child(rec.title.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(p.muted)
                            .child(format!("{elapsed}{steps_label}")),
                    )
            }))
    }

    fn render_subagent_inspector(
        &self,
        rec: &SubagentRecord,
        p: crate::theme::Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (glyph, color) = Self::subagent_status_visual(rec.state, p);
        let elapsed = Self::subagent_elapsed(rec);
        let sub_id_stop = rec.id.clone();
        let child_sid = rec.child_session.clone();
        let output_body = rec
            .result
            .clone()
            .or_else(|| rec.live_text.clone());

        v_flex()
            .flex_shrink_0()
            .max_h(px(300.0))
            .bg(p.bg)
            .border_b_1()
            .border_color(p.accent.opacity(0.6))
            .child(
                h_flex()
                    .flex_shrink_0()
                    .px_4()
                    .py_2()
                    .gap_2()
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
                            .items_center()
                            .child(div().text_xs().text_color(color).child(glyph))
                            .child(Tag::secondary().small().child(rec.role.clone()))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_sm()
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .truncate()
                                    .child(rec.title.clone()),
                            ),
                    )
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .gap_2()
                            .items_center()
                            .when_some(rec.model.clone(), |row, m| {
                                row.child(Tag::secondary().small().child(m))
                            })
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(color)
                                    .child(format!("{} · {elapsed}", rec.state.label())),
                            )
                            .when_some(
                                child_sid.filter(|_| self.engine == ChatEngine::OpenCode),
                                |row, sid| {
                                    row.child(
                                        Button::new("subagent-open-session")
                                            .xsmall()
                                            .label("Open session")
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.show_chat(Some(sid.clone()), window, cx);
                                            })),
                                    )
                                },
                            )
                            .when(rec.state.is_active() && rec.cancellable, |row| {
                                row.child(
                                    Button::new("subagent-inspector-stop")
                                        .xsmall()
                                        .danger()
                                        .label("Stop")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.cancel_subagent(sub_id_stop.clone(), cx);
                                        })),
                                )
                            })
                            .child(
                                Button::new("subagent-inspector-close")
                                    .xsmall()
                                    .ghost()
                                    .label("Close")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.inspected_subagent = None;
                                        this.focus_composer(window, cx);
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .child(
                div()
                    .id("subagent-inspector-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_4()
                    .py_2()
                    .child(
                        v_flex()
                            .w_full()
                            .gap_2()
                            .when(!rec.prompt.is_empty(), |col| {
                                col.child(
                                    v_flex()
                                        .gap_1()
                                        .child(
                                            div()
                                                .text_xs()
                                                .font_weight(gpui_kit::FontWeight::MEDIUM)
                                                .text_color(p.muted)
                                                .child("Task"),
                                        )
                                        .child(
                                            div()
                                                .px_2()
                                                .py_1()
                                                .rounded(px(6.0))
                                                .bg(p.chip)
                                                .text_xs()
                                                .child(rec.prompt.clone()),
                                        ),
                                )
                            })
                            .when(!rec.steps.is_empty(), |col| {
                                col.child(
                                    v_flex()
                                        .gap_1()
                                        .child(
                                            div()
                                                .text_xs()
                                                .font_weight(gpui_kit::FontWeight::MEDIUM)
                                                .text_color(p.muted)
                                                .child(format!("Steps ({})", rec.steps.len())),
                                        )
                                        .children(rec.steps.iter().map(|step| {
                                            let st_color = match step.status.as_str() {
                                                "failed" => p.bad,
                                                "in_progress" => p.accent,
                                                _ => p.good,
                                            };
                                            h_flex()
                                                .px_2()
                                                .py(px(3.0))
                                                .gap_2()
                                                .items_center()
                                                .rounded(px(5.0))
                                                .bg(p.panel)
                                                .border_1()
                                                .border_color(p.line)
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                                        .child(step.verb.clone()),
                                                )
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .text_xs()
                                                        .font_family("Menlo")
                                                        .text_color(p.muted)
                                                        .truncate()
                                                        .child(step.subject.clone()),
                                                )
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(st_color)
                                                        .child(step.status.clone()),
                                                )
                                        })),
                                )
                            })
                            .when_some(rec.error.clone(), |col, err| {
                                col.child(
                                    div()
                                        .px_2()
                                        .py_1()
                                        .rounded(px(6.0))
                                        .bg(p.bad.opacity(0.14))
                                        .text_xs()
                                        .text_color(p.bad)
                                        .child(err),
                                )
                            })
                            .when_some(output_body, |col, text| {
                                col.child(
                                    v_flex()
                                        .gap_1()
                                        .child(
                                            div()
                                                .text_xs()
                                                .font_weight(gpui_kit::FontWeight::MEDIUM)
                                                .text_color(p.muted)
                                                .child(if rec.result.is_some() {
                                                    "Result"
                                                } else {
                                                    "Live output"
                                                }),
                                        )
                                        .child(
                                            div()
                                                .px_3()
                                                .py_2()
                                                .rounded(px(6.0))
                                                .bg(p.panel)
                                                .border_1()
                                                .border_color(p.line)
                                                .text_xs()
                                                .child(
                                                    TextView::markdown(
                                                        ("subagent-md", rec.started_ms as usize),
                                                        text,
                                                    )
                                                    .selectable(true),
                                                ),
                                        ),
                                )
                            }),
                    ),
            )
    }

    // --- Host-side Git & GitHub PR actions (outside the agent's sandbox) ---

    fn project_root(&self) -> Option<std::path::PathBuf> {
        let pid = self.selected_project.as_deref()?;
        Some(self.backend.app.project(pid)?.main().to_path_buf())
    }

    fn refresh_git(&mut self, check_pr: bool, cx: &mut Context<Self>) {
        let Some(root) = self.project_root() else {
            self.git_status = None;
            cx.notify();
            return;
        };
        self.git_gen += 1;
        let mine = self.git_gen;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let root_task = root.clone();
        self.backend.tokio.spawn(async move {
            let st = git::status(&root_task).await;
            let _ = tx.send(st);
        });
        cx.spawn(async move |this, cx| {
            let Ok(mut st) = rx.await else {
                return;
            };
            let should_check_pr =
                check_pr && st.is_repo && st.has_remote && st.gh_available && !st.detached;
            let _ = this.update(cx, |this, cx| {
                if this.git_gen != mine {
                    return;
                }
                // Preserve known PR info on the same branch during fast local refreshes.
                if !should_check_pr
                    && let Some(prev) = &this.git_status
                    && prev.branch == st.branch
                {
                    st.open_pr = prev.open_pr.clone();
                }
                this.git_status = if st.is_repo { Some(st) } else { None };
                cx.notify();
            });
            if should_check_pr {
                let (pr_tx, pr_rx) = tokio::sync::oneshot::channel();
                let tokio_handle = this
                    .update(cx, |this, _| this.backend.tokio.handle().clone())
                    .ok();
                if let Some(handle) = tokio_handle {
                    handle.spawn(async move {
                        let pr = git::current_pr(&root).await;
                        let _ = pr_tx.send(pr);
                    });
                    if let Ok(pr) = pr_rx.await {
                        let _ = this.update(cx, |this, cx| {
                            if this.git_gen != mine {
                                return;
                            }
                            if let Some(gs) = &mut this.git_status {
                                gs.open_pr = pr;
                            }
                            cx.notify();
                        });
                    }
                }
            }
        })
        .detach();
    }

    fn toggle_commit_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.git_panel, Some(GitPanel::Commit { .. })) {
            self.git_panel = None;
            self.focus_composer(window, cx);
            cx.notify();
            return;
        }
        self.git_notice = None;
        let message_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Commit message (e.g. Fix login redirect)")
        });
        let sub = cx.subscribe_in(
            &message_input,
            window,
            |this, _, event: &InputEvent, _window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.submit_commit(false, cx);
                }
            },
        );
        message_input.update(cx, |s, cx| s.focus(window, cx));
        self.git_panel = Some(GitPanel::Commit {
            message_input,
            _sub: sub,
        });
        cx.notify();
    }

    fn toggle_pr_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.git_panel, Some(GitPanel::Pr { .. })) {
            self.git_panel = None;
            self.focus_composer(window, cx);
            cx.notify();
            return;
        }
        self.git_notice = None;
        let title_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Pull request title")
        });
        let body_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Short summary of what changed (optional)")
                .rows(3)
                .auto_grow(2, 6)
        });
        let body_for_title = body_input.clone();
        let sub = cx.subscribe_in(
            &title_input,
            window,
            move |_this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    body_for_title.update(cx, |s, cx| s.focus(window, cx));
                }
            },
        );
        title_input.update(cx, |s, cx| s.focus(window, cx));
        self.git_panel = Some(GitPanel::Pr {
            title_input,
            body_input,
            draft: false,
            _sub: sub,
        });
        cx.notify();
        // Automatically pre-fill title & summary from branch commits (uses model if loaded).
        self.draft_pr_with_ai(window, cx);
    }

    fn draft_commit_with_ai(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.git_drafting {
            return;
        }
        let Some(root) = self.project_root() else {
            return;
        };
        let Some(GitPanel::Commit { message_input, .. }) = &self.git_panel else {
            return;
        };
        let message_input = message_input.clone();
        self.git_drafting = true;
        self.git_notice = None;
        cx.notify();

        let app = self.backend.app.clone();
        let preferred = self.selected_model.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.backend.tokio.spawn(async move {
            let res = git::generate_commit_message(&app, &root, preferred.as_deref())
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(res);
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(res) = rx.await else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.git_drafting = false;
                match res {
                    Ok(msg) => {
                        message_input.update(cx, |state, cx| {
                            state.set_value(msg, window, cx);
                        });
                    }
                    Err(err) => {
                        this.git_notice = Some(GitNotice {
                            text: err,
                            url: None,
                            is_error: true,
                        });
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn draft_pr_with_ai(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.git_drafting {
            return;
        }
        let Some(root) = self.project_root() else {
            return;
        };
        let Some(GitPanel::Pr {
            title_input,
            body_input,
            ..
        }) = &self.git_panel
        else {
            return;
        };
        let title_input = title_input.clone();
        let body_input = body_input.clone();
        self.git_drafting = true;
        cx.notify();

        let app = self.backend.app.clone();
        let preferred = self.selected_model.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.backend.tokio.spawn(async move {
            let res = git::generate_pr_draft(&app, &root, preferred.as_deref())
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(res);
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(res) = rx.await else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.git_drafting = false;
                match res {
                    Ok(draft) => {
                        title_input.update(cx, |state, cx| {
                            state.set_value(draft.title, window, cx);
                        });
                        body_input.update(cx, |state, cx| {
                            state.set_value(draft.body, window, cx);
                        });
                    }
                    Err(err) => {
                        this.git_notice = Some(GitNotice {
                            text: err,
                            url: None,
                            is_error: true,
                        });
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn submit_commit(&mut self, also_push: bool, cx: &mut Context<Self>) {
        if self.git_busy.is_some() {
            return;
        }
        let Some(root) = self.project_root() else {
            return;
        };
        let message = match &self.git_panel {
            Some(GitPanel::Commit { message_input, .. }) => {
                message_input.read(cx).value().trim().to_string()
            }
            _ => return,
        };
        if message.is_empty() {
            self.git_notice = Some(GitNotice {
                text: "Enter a commit message first.".into(),
                url: None,
                is_error: true,
            });
            cx.notify();
            return;
        }
        self.git_busy = Some(if also_push {
            "Committing & pushing…".into()
        } else {
            "Committing…".into()
        });
        self.git_notice = None;
        cx.notify();

        let (tx, rx) = tokio::sync::oneshot::channel();
        self.backend.tokio.spawn(async move {
            let res: Result<String, String> = async {
                let c = git::commit(&root, &message, true)
                    .await
                    .map_err(|e| format!("{e:#}"))?;
                if also_push {
                    let p = git::push(&root).await.map_err(|e| format!("{e:#}"))?;
                    Ok(format!("Committed {} and pushed to {}/{}", c.hash, p.remote, p.branch))
                } else {
                    Ok(format!("Committed {} — {}", c.hash, c.summary))
                }
            }
            .await;
            let _ = tx.send(res);
        });
        cx.spawn(async move |this, cx| {
            let Ok(res) = rx.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.git_busy = None;
                match res {
                    Ok(text) => {
                        this.git_panel = None;
                        this.git_notice = Some(GitNotice {
                            text,
                            url: None,
                            is_error: false,
                        });
                        this.refresh_git(also_push, cx);
                    }
                    Err(err) => {
                        this.git_notice = Some(GitNotice {
                            text: err,
                            url: None,
                            is_error: true,
                        });
                        this.refresh_git(false, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn run_git_push(&mut self, cx: &mut Context<Self>) {
        if self.git_busy.is_some() {
            return;
        }
        let Some(root) = self.project_root() else {
            return;
        };
        self.git_busy = Some("Pushing…".into());
        self.git_notice = None;
        cx.notify();

        let (tx, rx) = tokio::sync::oneshot::channel();
        self.backend.tokio.spawn(async move {
            let res = git::push(&root).await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(res);
        });
        cx.spawn(async move |this, cx| {
            let Ok(res) = rx.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.git_busy = None;
                match res {
                    Ok(p) => {
                        let suffix = if p.set_upstream { " (upstream set)" } else { "" };
                        this.git_notice = Some(GitNotice {
                            text: format!("Pushed {} to {}{suffix}", p.branch, p.remote),
                            url: None,
                            is_error: false,
                        });
                        this.refresh_git(true, cx);
                    }
                    Err(err) => {
                        this.git_notice = Some(GitNotice {
                            text: err,
                            url: None,
                            is_error: true,
                        });
                        this.refresh_git(false, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn submit_pr(&mut self, cx: &mut Context<Self>) {
        if self.git_busy.is_some() {
            return;
        }
        let Some(root) = self.project_root() else {
            return;
        };
        let (title, body, draft) = match &self.git_panel {
            Some(GitPanel::Pr {
                title_input,
                body_input,
                draft,
                ..
            }) => (
                title_input.read(cx).value().trim().to_string(),
                body_input.read(cx).value().trim().to_string(),
                *draft,
            ),
            _ => return,
        };
        if title.is_empty() {
            self.git_notice = Some(GitNotice {
                text: "Enter a pull request title first.".into(),
                url: None,
                is_error: true,
            });
            cx.notify();
            return;
        }
        self.git_busy = Some("Creating PR…".into());
        self.git_notice = None;
        cx.notify();

        let (tx, rx) = tokio::sync::oneshot::channel();
        self.backend.tokio.spawn(async move {
            let res = git::create_pr(&root, &title, &body, draft)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(res);
        });
        cx.spawn(async move |this, cx| {
            let Ok(res) = rx.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.git_busy = None;
                match res {
                    Ok(pr) => {
                        this.git_panel = None;
                        let label = if pr.number > 0 {
                            format!("Created PR #{}: {}", pr.number, pr.title)
                        } else {
                            format!("Created PR: {}", pr.title)
                        };
                        let url = (!pr.url.is_empty()).then_some(pr.url.clone());
                        if let Some(gs) = &mut this.git_status {
                            gs.open_pr = Some(pr);
                        }
                        this.git_notice = Some(GitNotice {
                            text: label,
                            url,
                            is_error: false,
                        });
                        this.refresh_git(true, cx);
                    }
                    Err(err) => {
                        this.git_notice = Some(GitNotice {
                            text: err,
                            url: None,
                            is_error: true,
                        });
                        this.refresh_git(false, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn render_git_bar(
        &self,
        st: &GitStatus,
        p: crate::theme::Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let dirty = st.dirty_count();
        let branch_label = if st.detached {
            "detached HEAD".to_string()
        } else {
            st.branch.clone().unwrap_or_else(|| "HEAD".into())
        };
        let git_busy = self.git_busy.is_some();
        let can_commit = dirty > 0 && !git_busy;
        let can_push = st.has_remote && !st.detached && !git_busy;
        let push_highlighted = dirty == 0 && (st.ahead > 0 || st.upstream.is_none()) && can_push;

        let commit_label = if dirty > 0 {
            format!("Commit ({dirty})")
        } else {
            "Commit".into()
        };
        let push_label = if st.ahead > 0 {
            format!("Push ↑{}", st.ahead)
        } else if st.upstream.is_none() && st.has_remote {
            "Push branch".into()
        } else {
            "Push".into()
        };
        let open_pr = st.open_pr.clone();
        let can_pr = st.has_remote && !st.detached && !git_busy;
        let branch_focus = window
            .use_keyed_state("git-branch-focus", cx, |_, cx| cx.focus_handle())
            .read(cx)
            .clone();
        let branch_focused = branch_focus.is_focused(window);

        h_flex()
            .flex_shrink_0()
            .px_4()
            .py(px(5.0))
            .gap_2()
            .items_center()
            .justify_between()
            .bg(p.bg)
            .border_b_1()
            .border_color(p.line)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .min_w_0()
                    .child(
                        div()
                            .id("git-branch-pill")
                            .track_focus(&branch_focus)
                            .tab_stop(true)
                            .tab_index(0)
                            .cursor_pointer()
                            .px_2()
                            .py(px(2.0))
                            .rounded(px(5.0))
                            .border_1()
                            .border_color(if branch_focused { p.accent } else { p.chip })
                            .bg(p.chip)
                            .text_xs()
                            .font_family("Menlo")
                            .text_color(p.text)
                            .child(format!("⎇ {branch_label}"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.refresh_git(true, cx);
                            }))
                            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                                let mods = &ev.keystroke.modifiers;
                                if !mods.platform
                                    && !mods.control
                                    && !mods.alt
                                    && matches!(ev.keystroke.key.as_str(), "enter" | "space")
                                {
                                    cx.stop_propagation();
                                    this.refresh_git(true, cx);
                                }
                            })),
                    )
                    .when(dirty > 0, |row| {
                        row.child(
                            div()
                                .text_xs()
                                .text_color(p.warn)
                                .child(format!("{dirty} changed")),
                        )
                    })
                    .when(dirty == 0 && st.ahead == 0 && st.behind == 0, |row| {
                        row.child(div().text_xs().text_color(p.muted).child("Clean"))
                    })
                    .when(st.ahead > 0, |row| {
                        row.child(
                            div()
                                .text_xs()
                                .text_color(p.good)
                                .child(format!("↑{}", st.ahead)),
                        )
                    })
                    .when(st.behind > 0, |row| {
                        row.child(
                            div()
                                .text_xs()
                                .text_color(p.muted)
                                .child(format!("↓{}", st.behind)),
                        )
                    })
                    .when_some(self.git_busy.clone(), |row, msg| {
                        row.child(div().text_xs().text_color(p.accent).child(msg))
                    }),
            )
            .child(
                h_flex()
                    .flex_shrink_0()
                    .gap_1()
                    .items_center()
                    .child(
                        Button::new("git-commit-btn")
                            .xsmall()
                            .when(dirty > 0, |b| b.primary())
                            .disabled(!can_commit)
                            .label(commit_label)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_commit_panel(window, cx);
                            })),
                    )
                    .child(
                        Button::new("git-push-btn")
                            .xsmall()
                            .when(push_highlighted, |b| b.primary())
                            .disabled(!can_push)
                            .label(push_label)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.run_git_push(cx);
                            })),
                    )
                    .child(match open_pr {
                        Some(pr) => {
                            let url = pr.url.clone();
                            let label = if pr.number > 0 {
                                format!("PR #{} ↗", pr.number)
                            } else {
                                "Open PR ↗".into()
                            };
                            Button::new("git-pr-btn")
                                .xsmall()
                                .label(label)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Err(e) = files::open_url(&url) {
                                        this.git_notice = Some(GitNotice {
                                            text: format!("Couldn't open PR link: {e}"),
                                            url: None,
                                            is_error: true,
                                        });
                                        cx.notify();
                                    }
                                }))
                        }
                        None => Button::new("git-pr-btn")
                            .xsmall()
                            .disabled(!can_pr)
                            .label("PR")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_pr_panel(window, cx);
                            })),
                    }),
            )
    }

    fn render_git_panel(
        &self,
        panel: &GitPanel,
        st: Option<&GitStatus>,
        p: crate::theme::Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let has_model = self.loaded_explain_model().is_some();
        let busy = self.git_busy.is_some();

        match panel {
            GitPanel::Commit { message_input, .. } => {
                let files_list = st.map(|s| s.dirty_files.as_slice()).unwrap_or(&[]);
                let shown: Vec<_> = files_list.iter().take(8).cloned().collect();
                let extra = files_list.len().saturating_sub(shown.len());
                let can_push = st.is_some_and(|s| s.has_remote && !s.detached);

                v_flex()
                    .flex_shrink_0()
                    .px_4()
                    .py_3()
                    .gap_2()
                    .bg(p.panel)
                    .border_b_1()
                    .border_color(p.line)
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .child(format!(
                                        "Commit {} changed file{}",
                                        files_list.len(),
                                        if files_list.len() == 1 { "" } else { "s" }
                                    )),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(p.muted)
                                    .child("Stages all project changes and commits on host"),
                            ),
                    )
                    .when(!shown.is_empty(), |col| {
                        col.child(
                            h_flex()
                                .flex_wrap()
                                .gap_1()
                                .children(shown.into_iter().map(|f| {
                                    div()
                                        .px_2()
                                        .py(px(1.0))
                                        .rounded(px(4.0))
                                        .bg(p.chip)
                                        .text_xs()
                                        .font_family("Menlo")
                                        .text_color(p.muted)
                                        .child(format!("{} {}", f.status, f.path))
                                }))
                                .when(extra > 0, |row| {
                                    row.child(
                                        div()
                                            .text_xs()
                                            .text_color(p.muted)
                                            .child(format!("+{extra} more")),
                                    )
                                }),
                        )
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().flex_1().child(Input::new(message_input).small()))
                            .when(has_model, |row| {
                                row.child(
                                    Button::new("git-ai-commit")
                                        .small()
                                        .disabled(self.git_drafting || busy)
                                        .label(if self.git_drafting {
                                            "Drafting…"
                                        } else {
                                            "Draft message"
                                        })
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.draft_commit_with_ai(window, cx);
                                        })),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("git-commit-cancel")
                                    .small()
                                    .ghost()
                                    .label("Cancel")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.git_panel = None;
                                        this.focus_composer(window, cx);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("git-commit-only")
                                    .small()
                                    .disabled(busy)
                                    .label("Commit")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.submit_commit(false, cx);
                                    })),
                            )
                            .when(can_push, |row| {
                                row.child(
                                    Button::new("git-commit-push")
                                        .small()
                                        .primary()
                                        .disabled(busy)
                                        .label("Commit & Push")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.submit_commit(true, cx);
                                        })),
                                )
                            }),
                    )
            }
            GitPanel::Pr {
                title_input,
                body_input,
                draft,
                ..
            } => {
                let is_draft = *draft;
                let will_push = st.is_some_and(|s| s.upstream.is_none() || s.ahead > 0);
                let gh_ok = st.is_some_and(|s| s.gh_available);

                v_flex()
                    .flex_shrink_0()
                    .px_4()
                    .py_3()
                    .gap_2()
                    .bg(p.panel)
                    .border_b_1()
                    .border_color(p.line)
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .child("Create Pull Request"),
                            )
                            .child(
                                h_flex()
                                    .gap_1()
                                    .items_center()
                                    .when(has_model, |row| {
                                        row.child(
                                            Button::new("git-ai-pr")
                                                .xsmall()
                                                .disabled(self.git_drafting || busy)
                                                .label(if self.git_drafting {
                                                    "Drafting…"
                                                } else {
                                                    "Draft with AI"
                                                })
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.draft_pr_with_ai(window, cx);
                                                })),
                                        )
                                    })
                                    .child(
                                        Button::new("git-pr-draft-toggle")
                                            .xsmall()
                                            .when(is_draft, |b| b.primary())
                                            .label(if is_draft {
                                                "Draft PR: On"
                                            } else {
                                                "Draft PR: Off"
                                            })
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                if let Some(GitPanel::Pr { draft, .. }) =
                                                    &mut this.git_panel
                                                {
                                                    *draft = !*draft;
                                                    cx.notify();
                                                }
                                            })),
                                    ),
                            ),
                    )
                    .when(!gh_ok, |col| {
                        col.child(
                            div()
                                .text_xs()
                                .text_color(p.warn)
                                .child("GitHub CLI (`gh`) not found on PATH. Install it with `brew install gh` and run `gh auth login`."),
                        )
                    })
                    .child(Input::new(title_input).small())
                    .child(Textarea::new(body_input))
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(p.muted)
                                    .child(if will_push {
                                        "Pushes this branch first, then opens the PR via gh."
                                    } else {
                                        "Opens a pull request on GitHub via gh."
                                    }),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        Button::new("git-pr-cancel")
                                            .small()
                                            .ghost()
                                            .label("Cancel")
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.git_panel = None;
                                                this.focus_composer(window, cx);
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        Button::new("git-pr-submit")
                                            .small()
                                            .primary()
                                            .disabled(busy || !gh_ok)
                                            .label(if is_draft {
                                                "Create Draft PR"
                                            } else {
                                                "Create PR"
                                            })
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.submit_pr(cx);
                                            })),
                                    ),
                            ),
                    )
            }
        }
    }

    fn render_projects_popout(
        &self,
        p: crate::theme::Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_dark = cx.theme().mode == ThemeMode::Dark;
        let query = self.projects_popout_search.read(cx).value().to_string();
        let projects = self.ordered_projects(&query);
        let selected = self.projects_popout_selected;
        let all_active = self.filter_project.is_none();

        div()
            .id("projects-popout-backdrop")
            .absolute()
            .inset_0()
            .on_click(cx.listener(|this, _, window, cx| {
                this.close_projects_popout(window, cx);
            }))
            .child(
                v_flex()
                    .id("projects-popout-card")
                    .absolute()
                    .top(px(76.0))
                    .left(px(12.0))
                    .w(px(264.0))
                    .max_h(px(420.0))
                    .p_2()
                    .gap_1()
                    .rounded(px(10.0))
                    .bg(p.panel)
                    .border_1()
                    .border_color(p.line)
                    .shadow_lg()
                    .on_click(|_, _, cx| {
                        cx.stop_propagation();
                    })
                    .child(Input::new(&self.projects_popout_search).small())
                    .child(
                        div()
                            .id("popout-add-project")
                            .w_full()
                            .px_2()
                            .py_1p5()
                            .rounded(px(6.0))
                            .cursor_pointer()
                            .when(selected == 0, |d| d.bg(p.chip))
                            .hover(|d| d.bg(p.chip))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_projects_popout = false;
                                cx.notify();
                                choose_project_folder("Add a project folder", true, cx);
                            }))
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        h_flex()
                                            .size(px(20.0))
                                            .rounded(px(4.0))
                                            .items_center()
                                            .justify_center()
                                            .bg(p.chip)
                                            .text_xs()
                                            .font_weight(gpui_kit::FontWeight::BOLD)
                                            .child("+"),
                                    )
                                    .child(div().text_sm().child("Add project")),
                            ),
                    )
                    .child(
                        div()
                            .id("popout-all-projects")
                            .w_full()
                            .px_2()
                            .py_1p5()
                            .rounded(px(6.0))
                            .cursor_pointer()
                            .when(selected == 1 || all_active, |d| d.bg(p.chip))
                            .hover(|d| d.bg(p.chip))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.filter_project = None;
                                this.close_projects_popout(window, cx);
                            }))
                            .child(
                                h_flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_2()
                                    .child(
                                        h_flex()
                                            .items_center()
                                            .gap_2()
                                            .child(
                                                h_flex()
                                                    .size(px(20.0))
                                                    .rounded(px(4.0))
                                                    .items_center()
                                                    .justify_center()
                                                    .bg(p.chip)
                                                    .text_xs()
                                                    .child("▤"),
                                            )
                                            .child(div().text_sm().child("All projects")),
                                    )
                                    .when(all_active, |row| {
                                        row.child(
                                            div()
                                                .text_xs()
                                                .font_weight(gpui_kit::FontWeight::BOLD)
                                                .text_color(p.accent)
                                                .child("✓"),
                                        )
                                    }),
                            ),
                    )
                    .child(div().w_full().h(px(1.0)).bg(p.line).my(px(2.0)))
                    .child(
                        div()
                            .id("popout-projects-scroll")
                            .flex_1()
                            .min_h_0()
                            .max_h(px(280.0))
                            .overflow_y_scroll()
                            .child(
                                v_flex()
                                    .w_full()
                                    .gap_0p5()
                                    .when(projects.is_empty(), |col| {
                                        col.child(
                                            div()
                                                .px_2()
                                                .py_2()
                                                .text_xs()
                                                .text_color(p.muted)
                                                .child("No matching projects"),
                                        )
                                    })
                                    .children(projects.into_iter().enumerate().map(
                                        |(i, proj)| {
                                            let row_idx = i + 2;
                                            let pid = proj.id.clone();
                                            let is_filtered = self.filter_project.as_deref()
                                                == Some(proj.id.as_str());
                                            let (bg, fg) = project_badge_colors(&proj.id, is_dark);
                                            let mono = project_monogram(&proj.name);
                                            div()
                                                .id(("popout-proj", i))
                                                .w_full()
                                                .px_2()
                                                .py_1p5()
                                                .rounded(px(6.0))
                                                .cursor_pointer()
                                                .when(selected == row_idx || is_filtered, |d| {
                                                    d.bg(p.chip)
                                                })
                                                .hover(|d| d.bg(p.chip))
                                                .on_click(cx.listener(
                                                    move |this, _, window, cx| {
                                                        this.filter_project = Some(pid.clone());
                                                        this.close_projects_popout(window, cx);
                                                    },
                                                ))
                                                .child(
                                                    h_flex()
                                                        .w_full()
                                                        .items_center()
                                                        .justify_between()
                                                        .gap_2()
                                                        .child(
                                                            h_flex()
                                                                .flex_1()
                                                                .min_w_0()
                                                                .items_center()
                                                                .gap_2()
                                                                .child(
                                                                    h_flex()
                                                                        .size(px(20.0))
                                                                        .flex_shrink_0()
                                                                        .rounded(px(4.0))
                                                                        .items_center()
                                                                        .justify_center()
                                                                        .bg(bg)
                                                                        .text_color(fg)
                                                                        .text_size(px(9.5))
                                                                        .font_weight(
                                                                            gpui_kit::FontWeight::BOLD,
                                                                        )
                                                                        .child(mono),
                                                                )
                                                                .child(
                                                                    div()
                                                                        .flex_1()
                                                                        .min_w_0()
                                                                        .text_sm()
                                                                        .truncate()
                                                                        .child(proj.name),
                                                                ),
                                                        )
                                                        .when(is_filtered, |row| {
                                                            row.child(
                                                                div()
                                                                    .text_xs()
                                                                    .font_weight(
                                                                        gpui_kit::FontWeight::BOLD,
                                                                    )
                                                                    .text_color(p.accent)
                                                                    .child("✓"),
                                                            )
                                                        }),
                                                )
                                        },
                                    )),
                            ),
                    ),
            )
    }

    fn render_new_chat_modal(
        &self,
        p: crate::theme::Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_dark = cx.theme().mode == ThemeMode::Dark;
        let query = self.new_chat_search.read(cx).value().to_string();
        let projects = self.ordered_projects(&query);
        let selected = self.new_chat_selected;
        let add_idx = projects.len();

        h_flex()
            .id("new-chat-modal-backdrop")
            .absolute()
            .inset_0()
            .bg(gpui_kit::hsla(0.0, 0.0, 0.0, 0.5))
            .items_start()
            .justify_center()
            .pt(px(72.0))
            .on_click(cx.listener(|this, _, window, cx| {
                this.close_new_chat_modal(window, cx);
            }))
            .child(
                v_flex()
                    .id("new-chat-modal-card")
                    .w(px(520.0))
                    .max_h(px(460.0))
                    .rounded(px(12.0))
                    .bg(p.panel)
                    .border_1()
                    .border_color(p.line)
                    .shadow_lg()
                    .overflow_hidden()
                    .on_click(|_, _, cx| {
                        cx.stop_propagation();
                    })
                    // Search header
                    .child(
                        h_flex()
                            .w_full()
                            .px_3()
                            .py_2p5()
                            .gap_2()
                            .items_center()
                            .border_b_1()
                            .border_color(p.line)
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .text_color(p.muted)
                                    .child("New chat"),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(Input::new(&self.new_chat_search).small()),
                            ),
                    )
                    // Section label + project rows
                    .child(
                        div()
                            .px_3()
                            .pt_2()
                            .pb_1()
                            .text_xs()
                            .text_color(p.muted)
                            .child("Choose a project for this new chat"),
                    )
                    .child(
                        div()
                            .id("new-chat-modal-list")
                            .flex_1()
                            .min_h_0()
                            .max_h(px(320.0))
                            .overflow_y_scroll()
                            .px_2()
                            .pb_2()
                            .child(
                                v_flex()
                                    .w_full()
                                    .gap_0p5()
                                    .children(projects.into_iter().enumerate().map(|(i, proj)| {
                                        let pid = proj.id.clone();
                                        let is_current = self.selected_project.as_deref()
                                            == Some(proj.id.as_str());
                                        let (bg, fg) = project_badge_colors(&proj.id, is_dark);
                                        let mono = project_monogram(&proj.name);
                                        let env_label = match proj.isolation {
                                            Isolation::MacosSandbox => "macOS sandbox",
                                            Isolation::LinuxVm => "Linux VM",
                                        };
                                        let path_str = proj.main().display().to_string();
                                        let subtitle = if is_current {
                                            format!("Current · {env_label} · {path_str}")
                                        } else {
                                            format!("{env_label} · {path_str}")
                                        };
                                        let shortcut = (i < 9).then(|| format!("⌘{}", i + 1));

                                        div()
                                            .id(("new-chat-proj", i))
                                            .w_full()
                                            .px_2p5()
                                            .py_2()
                                            .rounded(px(8.0))
                                            .cursor_pointer()
                                            .when(selected == i, |d| d.bg(p.chip))
                                            .hover(|d| d.bg(p.chip))
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.start_new_chat_in_project(
                                                    pid.clone(),
                                                    window,
                                                    cx,
                                                );
                                            }))
                                            .child(
                                                h_flex()
                                                    .w_full()
                                                    .items_center()
                                                    .justify_between()
                                                    .gap_3()
                                                    .child(
                                                        h_flex()
                                                            .flex_1()
                                                            .min_w_0()
                                                            .items_center()
                                                            .gap_2p5()
                                                            .child(
                                                                h_flex()
                                                                    .size(px(24.0))
                                                                    .flex_shrink_0()
                                                                    .rounded(px(5.0))
                                                                    .items_center()
                                                                    .justify_center()
                                                                    .bg(bg)
                                                                    .text_color(fg)
                                                                    .text_size(px(10.5))
                                                                    .font_weight(
                                                                        gpui_kit::FontWeight::BOLD,
                                                                    )
                                                                    .child(mono),
                                                            )
                                                            .child(
                                                                v_flex()
                                                                    .flex_1()
                                                                    .min_w_0()
                                                                    .child(
                                                                        div()
                                                                            .text_sm()
                                                                            .font_weight(
                                                                                gpui_kit::FontWeight::MEDIUM,
                                                                            )
                                                                            .truncate()
                                                                            .child(proj.name),
                                                                    )
                                                                    .child(
                                                                        div()
                                                                            .text_xs()
                                                                            .text_color(p.muted)
                                                                            .truncate()
                                                                            .child(subtitle),
                                                                    ),
                                                            ),
                                                    )
                                                    .when_some(shortcut, |row, sc| {
                                                        row.child(
                                                            div()
                                                                .flex_shrink_0()
                                                                .text_xs()
                                                                .font_family("Menlo")
                                                                .text_color(p.muted)
                                                                .child(sc),
                                                        )
                                                    }),
                                            )
                                    }))
                                    .child(
                                        div()
                                            .id("new-chat-add-project")
                                            .w_full()
                                            .px_2p5()
                                            .py_2()
                                            .mt_1()
                                            .rounded(px(8.0))
                                            .cursor_pointer()
                                            .border_t_1()
                                            .border_color(p.line)
                                            .when(selected == add_idx, |d| d.bg(p.chip))
                                            .hover(|d| d.bg(p.chip))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.show_new_chat_modal = false;
                                                cx.notify();
                                                choose_project_folder(
                                                    "Add a project folder",
                                                    true,
                                                    cx,
                                                );
                                            }))
                                            .child(
                                                h_flex()
                                                    .items_center()
                                                    .justify_between()
                                                    .child(
                                                        h_flex()
                                                            .items_center()
                                                            .gap_2p5()
                                                            .child(
                                                                h_flex()
                                                                    .size(px(24.0))
                                                                    .rounded(px(5.0))
                                                                    .items_center()
                                                                    .justify_center()
                                                                    .bg(p.chip)
                                                                    .text_xs()
                                                                    .font_weight(
                                                                        gpui_kit::FontWeight::BOLD,
                                                                    )
                                                                    .child("+"),
                                                            )
                                                            .child(
                                                                div()
                                                                    .text_sm()
                                                                    .child("Add new project…"),
                                                            ),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_xs()
                                                            .font_family("Menlo")
                                                            .text_color(p.muted)
                                                            .child("⌘⇧O"),
                                                    ),
                                            ),
                                    ),
                            ),
                    )
                    // Keyboard hints footer
                    .child(
                        h_flex()
                            .w_full()
                            .px_3()
                            .py_2()
                            .gap_4()
                            .items_center()
                            .bg(p.bg)
                            .border_t_1()
                            .border_color(p.line)
                            .text_xs()
                            .text_color(p.muted)
                            .child("↑ ↓ Navigate")
                            .child("Enter Select")
                            .child("⌘1–9 Quick pick")
                            .child("Esc Close"),
                    ),
            )
    }

    fn render_shortcuts_help(
        &self,
        p: crate::theme::Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let rows: &[(&str, &str)] = &[
            ("⌘N", "New chat (choose project)"),
            ("⌘⇧O", "Add project folder…"),
            ("⌘V / Drop / ⌘⇧I", "Paste, drag & drop, or attach screenshots/images"),
            ("⌘F", "Search chats"),
            ("⌘L / ⌘K", "Focus composer"),
            ("⌘[ / ⌘]  or  ⌘⌥↑ / ⌘⌥↓", "Previous / next chat"),
            ("↑ / ↓  (in chat list)", "Move between chats (Enter opens)"),
            ("↑ / ↓ / Tab / Enter", "Navigate & pick / slash commands"),
            ("⌘⇧P / ⌘⇧M / ⌘⇧R", "Projects filter popout / model / reasoning picker"),
            ("⌘⇧G", "Toggle Git commit panel"),
            ("⌘⇧A", "Archive current chat"),
            ("⌘↵  (in panels)", "Save skill / Commit & Push / Create PR"),
            ("⌘↵ / ⌘⌫  (on prompt)", "Allow / Deny pending tool approval"),
            ("⌘.  or  Esc", "Dismiss panel / stop generation"),
            ("PgUp / PgDn / Home / End", "Scroll message history"),
            ("Tab / ⇧Tab", "Move keyboard focus between controls"),
            ("⌘,", "Open Settings"),
        ];
        v_flex()
            .flex_shrink_0()
            .mx_4()
            .mb_2()
            .p_3()
            .gap_2()
            .rounded(px(8.0))
            .border_1()
            .border_color(p.accent.opacity(0.7))
            .bg(p.panel)
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .child("Keyboard shortcuts (⌘/)"),
                    )
                    .child(
                        Button::new("shortcuts-help-close")
                            .ghost()
                            .xsmall()
                            .label("Close (Esc)")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_shortcuts_help = false;
                                this.focus_composer(window, cx);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_x_4()
                    .gap_y_1()
                    .children(rows.iter().map(|(keys, desc)| {
                        h_flex()
                            .w(px(320.0))
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .px(px(5.0))
                                    .py(px(1.0))
                                    .rounded(px(4.0))
                                    .bg(p.chip)
                                    .text_xs()
                                    .font_family("Menlo")
                                    .text_color(p.text)
                                    .child(*keys),
                            )
                            .child(div().text_xs().text_color(p.muted).child(*desc))
                    })),
            )
    }
}

impl Render for ChatWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_scroll_state();
        self.sync_session_focus_handles(cx);

        let is_dark = cx.theme().mode == ThemeMode::Dark;
        let p = palette(is_dark);

        let proj = self
            .selected_project
            .as_deref()
            .and_then(|id| self.backend.app.project(id));
        let header_project_settings = proj.as_ref().map(|pr| {
            (
                pr.id.clone(),
                pr.name.clone(),
                pr.main().display().to_string(),
            )
        });

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
        let selected_spec_badge = self.selected_model.as_deref().and_then(|id| {
            loaded.iter().find_map(|m| {
                if m.id == id && matches!(m.state, LoadState::Running { .. }) {
                    m.speculative.clone()
                } else {
                    None
                }
            })
        });
        let selected_loading = self.selected_model.as_deref().and_then(|id| {
            loading_models.iter().find(|(mid, _, _, _)| mid == id).cloned()
        });
        let has_loading_models = !loading_models.is_empty();
        let has_model = self.selected_model.is_some();
        let vm_blocked_builtin = false;
        let can_send = !self.busy && proj.is_some() && has_model;

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
                WorkingPhase::StartingAgent => format!("Starting agent… {elapsed}"),
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

        let slash_query = self.active_slash_query(cx);

        let (active_sessions, archived_sessions) = self.filtered_sessions(cx);
        let project_count = self.backend.app.settings.lock().unwrap().projects.len();
        let search_empty = self.sidebar_search.read(cx).value().trim().is_empty();
        let archived_count = archived_sessions.len();
        let archived_expanded = self.show_archived
            || archived_sessions
                .iter()
                .any(|s| self.active_session.as_deref() == Some(s.id.as_str()));
        let rendered_active_sessions: Vec<gpui_kit::AnyElement> = active_sessions
            .iter()
            .enumerate()
            .map(|(i, s)| {
                self.render_session_row(i, s, false, p, window, cx)
                    .into_any_element()
            })
            .collect();
        let rendered_archived_sessions: Vec<gpui_kit::AnyElement> = if archived_expanded {
            archived_sessions
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    self.render_session_row(10_000 + i, s, true, p, window, cx)
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
            .map(|block| self.render_block(block, window, cx).into_any_element())
            .collect();

        let active_sid_for_arch = self.active_session.clone();
        let active_pid_for_arch = self.selected_project.clone();
        let active_not_archived = match (&active_pid_for_arch, &active_sid_for_arch) {
            (Some(pid), Some(sid)) => !self.is_archived(pid, sid),
            _ => false,
        };
        let archived_toggle_focus = window
            .use_keyed_state("toggle-archived-focus", cx, |_, cx| cx.focus_handle())
            .read(cx)
            .clone();
        let archived_toggle_focused = archived_toggle_focus.is_focused(window);
        let active_proj_badge = proj.as_ref().map(|pr| {
            let (bg, fg) = project_badge_colors(&pr.id, is_dark);
            (project_monogram(&pr.name), pr.name.clone(), pr.isolation, bg, fg)
        });

        let drop_accent = p.accent;
        let view_for_paste = cx.entity().downgrade();
        let pending_images_snapshot = self.pending_images.clone();
        let filter_proj_badge = self.filter_project.as_deref().and_then(|fid| {
            let pr = self.backend.app.project(fid)?;
            let (bg, fg) = project_badge_colors(&pr.id, is_dark);
            Some((project_monogram(&pr.name), pr.name, bg, fg))
        });
        let _ = project_count;

        h_flex()
            .id("chat-window-root")
            .relative()
            .size_full()
            .bg(p.bg)
            .text_color(p.text)
            .track_focus(&self.focus_handle)
            .key_context("ChatWindow")
            .can_drop(|value, _, _| {
                value.downcast_ref::<ExternalPaths>().is_some_and(|paths| {
                    paths
                        .paths()
                        .iter()
                        .any(|path| attachments::is_supported_image_path(path))
                })
            })
            .drag_over::<ExternalPaths>(move |style, paths, _, _| {
                if paths
                    .paths()
                    .iter()
                    .any(|path| attachments::is_supported_image_path(path))
                {
                    style
                        .border_2()
                        .border_color(drop_accent)
                        .bg(drop_accent.opacity(0.06))
                } else {
                    style
                }
            })
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.attach_image_paths(paths.paths(), window, cx);
            }))
            .on_action(cx.listener(|this, _: &NewChat, window, cx| {
                this.open_new_chat_modal(window, cx);
            }))
            .on_action(cx.listener(|_this, _: &AddProject, _, cx| {
                choose_project_folder("Add a project folder", true, cx);
            }))
            .on_action(cx.listener(|this, _: &AttachImage, window, cx| {
                this.choose_images(window, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusChatSearch, window, cx| {
                this.sidebar_search.update(cx, |s, cx| s.focus(window, cx));
            }))
            .on_action(cx.listener(|this, _: &FocusComposer, window, cx| {
                this.focus_composer(window, cx);
            }))
            .on_action(cx.listener(|this, _: &PrevChat, window, cx| {
                this.switch_adjacent_chat(-1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &NextChat, window, cx| {
                this.switch_adjacent_chat(1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusProjectPicker, window, cx| {
                if this.show_projects_popout {
                    this.close_projects_popout(window, cx);
                } else {
                    this.open_projects_popout(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &FocusModelPicker, window, cx| {
                this.model_select.update(cx, |s, cx| s.focus(window, cx));
            }))
            .on_action(cx.listener(|this, _: &FocusReasoningPicker, window, cx| {
                if this.selected_model.is_some() {
                    this.reasoning_select
                        .update(cx, |s, cx| s.focus(window, cx));
                }
            }))
            .on_action(cx.listener(|this, _: &ArchiveActiveChat, window, cx| {
                if let (Some(pid), Some(sid)) =
                    (this.selected_project.clone(), this.active_session.clone())
                    && !this.is_archived(&pid, &sid)
                {
                    this.archive_chat(pid, sid, true, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleGitCommit, window, cx| {
                if this
                    .git_status
                    .as_ref()
                    .is_some_and(|s| s.is_repo && s.dirty_count() > 0)
                    || matches!(this.git_panel, Some(GitPanel::Commit { .. }))
                {
                    this.toggle_commit_panel(window, cx);
                } else {
                    this.refresh_git(true, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &StopGeneration, _, cx| {
                if this.busy {
                    this.stop_current(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleShortcutsHelp, _, cx| {
                this.show_shortcuts_help = !this.show_shortcuts_help;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Dismiss, window, cx| {
                this.handle_dismiss(window, cx);
            }))
            .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                this.handle_capture_key_down(ev, window, cx);
            }))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                this.handle_key_down(ev, window, cx);
            }))
            // Sidebar (T3 Code-style unified multi-project chat cards)
            .child(
                v_flex()
                    .w(px(288.0))
                    .flex_shrink_0()
                    .h_full()
                    .p_3()
                    .gap_2()
                    .bg(p.panel)
                    .border_r_1()
                    .border_color(p.line)
                    // Top toolbar: wide Search input + New Chat overlay trigger
                    .child(
                        h_flex()
                            .w_full()
                            .flex_shrink_0()
                            .gap_1p5()
                            .items_center()
                            .child(div().flex_1().min_w_0().child(Input::new(&self.sidebar_search).small()))
                            .child(
                                Button::new("new-chat")
                                    .primary()
                                    .small()
                                    .icon(IconName::SquarePen)
                                    .tooltip("New chat — choose a project (⌘N)")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_new_chat_modal(window, cx);
                                    })),
                            ),
                    )
                    // Second row: searchable project-filter dropdown + explicit "+ Project" button
                    .child(
                        h_flex()
                            .w_full()
                            .flex_shrink_0()
                            .gap_1p5()
                            .items_center()
                            .child(match filter_proj_badge {
                                Some((mono, pname, bg, fg)) => h_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .items_center()
                                    .gap_1()
                                    .child(
                                        div()
                                            .id("project-filter-dropdown")
                                            .flex_1()
                                            .min_w_0()
                                            .cursor_pointer()
                                            .px_2()
                                            .py(px(4.0))
                                            .rounded(px(6.0))
                                            .bg(p.bg)
                                            .border_1()
                                            .border_color(if self.show_projects_popout {
                                                p.accent
                                            } else {
                                                p.accent.opacity(0.55)
                                            })
                                            .hover(|d| d.bg(p.chip))
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                if this.show_projects_popout {
                                                    this.close_projects_popout(window, cx);
                                                } else {
                                                    this.open_projects_popout(window, cx);
                                                }
                                            }))
                                            .child(
                                                h_flex()
                                                    .w_full()
                                                    .items_center()
                                                    .justify_between()
                                                    .gap_1p5()
                                                    .child(
                                                        h_flex()
                                                            .flex_1()
                                                            .min_w_0()
                                                            .items_center()
                                                            .gap_1p5()
                                                            .child(
                                                                h_flex()
                                                                    .size(px(16.0))
                                                                    .flex_shrink_0()
                                                                    .rounded(px(3.0))
                                                                    .items_center()
                                                                    .justify_center()
                                                                    .bg(bg)
                                                                    .text_color(fg)
                                                                    .text_size(px(8.5))
                                                                    .font_weight(
                                                                        gpui_kit::FontWeight::BOLD,
                                                                    )
                                                                    .child(mono),
                                                            )
                                                            .child(
                                                                div()
                                                                    .flex_1()
                                                                    .min_w_0()
                                                                    .text_xs()
                                                                    .truncate()
                                                                    .child(pname),
                                                            ),
                                                    )
                                                    .child(
                                                        div()
                                                            .flex_shrink_0()
                                                            .text_xs()
                                                            .text_color(p.muted)
                                                            .child("▾"),
                                                    ),
                                            ),
                                    )
                                    .child(
                                        Button::new("clear-project-filter")
                                            .ghost()
                                            .xsmall()
                                            .label("×")
                                            .tooltip("Show chats from all projects")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.filter_project = None;
                                                cx.notify();
                                            })),
                                    )
                                    .into_any_element(),
                                None => div()
                                    .id("project-filter-dropdown")
                                    .flex_1()
                                    .min_w_0()
                                    .cursor_pointer()
                                    .px_2()
                                    .py(px(4.0))
                                    .rounded(px(6.0))
                                    .bg(p.bg)
                                    .border_1()
                                    .border_color(if self.show_projects_popout {
                                        p.accent
                                    } else {
                                        p.line
                                    })
                                    .hover(|d| d.bg(p.chip))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        if this.show_projects_popout {
                                            this.close_projects_popout(window, cx);
                                        } else {
                                            this.open_projects_popout(window, cx);
                                        }
                                    }))
                                    .child(
                                        h_flex()
                                            .w_full()
                                            .items_center()
                                            .justify_between()
                                            .gap_1p5()
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .text_xs()
                                                    .text_color(p.text.opacity(0.9))
                                                    .truncate()
                                                    .child("All projects"),
                                            )
                                            .child(
                                                div()
                                                    .flex_shrink_0()
                                                    .text_xs()
                                                    .text_color(p.muted)
                                                    .child("▾"),
                                            ),
                                    )
                                    .into_any_element(),
                            })
                            .child(
                                Button::new("add-project-btn")
                                    .xsmall()
                                    .label("+ Project")
                                    .tooltip("Add a new project folder (⌘⇧O)")
                                    .on_click(|_, _, cx| {
                                        choose_project_folder("Add a project folder", true, cx);
                                    }),
                            ),
                    )
                    .when_some(
                        self.confirm_delete_chat.clone(),
                        |col, (cpid, cid, ctitle)| {
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
                                                    .on_click(cx.listener(
                                                        move |this, _, window, cx| {
                                                            this.delete_chat(
                                                                cpid.clone(),
                                                                cid.clone(),
                                                                window,
                                                                cx,
                                                            );
                                                        },
                                                    )),
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
                        },
                    )
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
                                    .gap_1p5()
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
                                    // Highlighted draft card when composing a brand new chat
                                    .when(self.active_session.is_none() && search_empty, |col| {
                                        col.when_some(
                                            active_proj_badge.clone(),
                                            |col, (mono, pname, iso, bg, fg)| {
                                                let env_label = match iso {
                                                    Isolation::MacosSandbox => "macOS sandbox",
                                                    Isolation::LinuxVm => "Linux VM",
                                                };
                                                col.child(
                                                    v_flex()
                                                        .flex_shrink_0()
                                                        .w_full()
                                                        .px_2p5()
                                                        .py_2()
                                                        .gap_1()
                                                        .rounded(px(8.0))
                                                        .border_1()
                                                        .border_color(p.accent.opacity(0.6))
                                                        .bg(p.chip)
                                                        .child(
                                                            h_flex()
                                                                .w_full()
                                                                .items_center()
                                                                .justify_between()
                                                                .gap_2()
                                                                .child(
                                                                    h_flex()
                                                                        .flex_1()
                                                                        .min_w_0()
                                                                        .items_center()
                                                                        .gap_1p5()
                                                                        .child(
                                                                            h_flex()
                                                                                .size(px(18.0))
                                                                                .flex_shrink_0()
                                                                                .rounded(px(4.0))
                                                                                .items_center()
                                                                                .justify_center()
                                                                                .bg(bg)
                                                                                .text_color(fg)
                                                                                .text_size(px(9.0))
                                                                                .font_weight(
                                                                                    gpui_kit::FontWeight::BOLD,
                                                                                )
                                                                                .child(mono),
                                                                        )
                                                                        .child(
                                                                            div()
                                                                                .flex_1()
                                                                                .min_w_0()
                                                                                .text_xs()
                                                                                .text_color(p.muted)
                                                                                .truncate()
                                                                                .child(pname),
                                                                        ),
                                                                )
                                                                .child(
                                                                    div()
                                                                        .text_xs()
                                                                        .text_color(p.accent)
                                                                        .child("Draft"),
                                                                ),
                                                        )
                                                        .child(
                                                            div()
                                                                .w_full()
                                                                .text_sm()
                                                                .font_weight(
                                                                    gpui_kit::FontWeight::SEMIBOLD,
                                                                )
                                                                .child("New chat"),
                                                        )
                                                        .child(
                                                            div()
                                                                .text_xs()
                                                                .text_color(p.muted)
                                                                .child(env_label),
                                                        ),
                                                )
                                            },
                                        )
                                    })
                                    .children(rendered_active_sessions)
                                    .when(
                                        active_sessions.is_empty() && !search_empty,
                                        |col| {
                                            col.child(
                                                div()
                                                    .px_2()
                                                    .py_3()
                                                    .text_xs()
                                                    .text_color(p.muted)
                                                    .child("No matching chats."),
                                            )
                                        },
                                    ),
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
                                        .track_focus(&archived_toggle_focus)
                                        .tab_stop(true)
                                        .tab_index(0)
                                        .cursor_pointer()
                                        .px_2()
                                        .py_1()
                                        .rounded(px(5.0))
                                        .border_1()
                                        .border_color(if archived_toggle_focused {
                                            p.accent
                                        } else {
                                            p.panel
                                        })
                                        .text_xs()
                                        .text_color(p.muted)
                                        .child(format!(
                                            "{} Archived ({archived_count})",
                                            if archived_expanded { "▾" } else { "▸" }
                                        ))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.show_archived = !this.show_archived;
                                            cx.notify();
                                        }))
                                        .on_key_down(cx.listener(
                                            |this, ev: &KeyDownEvent, _, cx| {
                                                let mods = &ev.keystroke.modifiers;
                                                if !mods.platform
                                                    && !mods.control
                                                    && !mods.alt
                                                    && matches!(
                                                        ev.keystroke.key.as_str(),
                                                        "enter" | "space"
                                                    )
                                                {
                                                    cx.stop_propagation();
                                                    this.show_archived = !this.show_archived;
                                                    cx.notify();
                                                }
                                            },
                                        )),
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
                        h_flex()
                            .w_full()
                            .flex_shrink_0()
                            .pt_2()
                            .gap_1()
                            .items_center()
                            .justify_between()
                            .border_t_1()
                            .border_color(p.line)
                            .child(
                                h_flex()
                                    .gap_1()
                                    .items_center()
                                    .child(
                                        Button::new("open-settings")
                                            .ghost()
                                            .small()
                                            .icon(IconName::Settings)
                                            .tooltip("Projects & settings (⌘,)")
                                            .on_click(|_, _, cx| open_settings(None, cx)),
                                    )
                                    .child(
                                        Button::new("open-setup")
                                            .ghost()
                                            .small()
                                            .icon(IconName::BookOpen)
                                            .tooltip("Setup guide")
                                            .on_click(|_, _, cx| open_setup(cx)),
                                    ),
                            )
                            .child(
                                Button::new("toggle-shortcuts-help")
                                    .ghost()
                                    .small()
                                    .icon(IconName::Keyboard)
                                    .tooltip("Keyboard shortcuts (⌘/)")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.show_shortcuts_help = !this.show_shortcuts_help;
                                        cx.notify();
                                    })),
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
                                    .items_center()
                                    .when_some(
                                        active_proj_badge.clone(),
                                        |row, (mono, pname, _, bg, fg)| {
                                            row.child(
                                                h_flex()
                                                    .flex_shrink_0()
                                                    .items_center()
                                                    .gap_1p5()
                                                    .child(
                                                        h_flex()
                                                            .size(px(20.0))
                                                            .rounded(px(4.0))
                                                            .items_center()
                                                            .justify_center()
                                                            .bg(bg)
                                                            .text_color(fg)
                                                            .text_size(px(10.0))
                                                            .font_weight(
                                                                gpui_kit::FontWeight::BOLD,
                                                            )
                                                            .child(mono),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_xs()
                                                            .text_color(p.muted)
                                                            .child(pname),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_xs()
                                                            .text_color(p.muted.opacity(0.6))
                                                            .child("/"),
                                                    ),
                                            )
                                        },
                                    )
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                            .truncate()
                                            .child(active_title),
                                    )
                                    .when_some(
                                        header_project_settings,
                                        |row, (pid, pname, ppath)| {
                                            row.child(
                                                Button::new("header-project-settings")
                                                    .ghost()
                                                    .xsmall()
                                                    .icon(IconName::Settings2)
                                                    .tooltip(format!(
                                                        "Project settings for {pname} ({ppath})"
                                                    ))
                                                    .on_click(move |_, _, cx| {
                                                        open_project_settings(pid.clone(), cx);
                                                    }),
                                            )
                                        },
                                    ),
                            )
                            .child(
                                h_flex()
                                    .flex_shrink_0()
                                    .gap_2()
                                    .items_center()
                                    .when(active_not_archived, |row| {
                                        let pid = active_pid_for_arch.clone().unwrap();
                                        let sid = active_sid_for_arch.clone().unwrap();
                                        row.child(
                                            Button::new("header-archive")
                                                .ghost()
                                                .small()
                                                .label("Archive")
                                                .tooltip("Archive this chat (⌘⇧A)")
                                                .on_click(cx.listener(move |this, _, window, cx| {
                                                    this.archive_chat(
                                                        pid.clone(),
                                                        sid.clone(),
                                                        true,
                                                        window,
                                                        cx,
                                                    );
                                                })),
                                        )
                                    })
                                    .when_some(usage_text, |row, usage| {
                                        row.child(Tag::secondary().small().child(usage))
                                    })
                                    .when_some(selected_spec_badge, |row, spec| {
                                        row.child(Tag::secondary().small().child(format!("⚡ {spec}")))
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
                                                .tooltip("Unload the running model to free memory")
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
                    // Git bar (Commit, Push, PR) — runs on host outside the sandbox
                    .when_some(self.git_status.as_ref().filter(|s| s.is_repo), |col, st| {
                        col.child(self.render_git_bar(st, p, window, cx))
                    })
                    .when_some(self.git_panel.as_ref(), |col, panel| {
                        col.child(self.render_git_panel(panel, self.git_status.as_ref(), p, cx))
                    })
                    .when_some(self.git_notice.clone(), |col, notice| {
                        col.child(
                            h_flex()
                                .flex_shrink_0()
                                .px_4()
                                .py(px(6.0))
                                .gap_2()
                                .items_center()
                                .justify_between()
                                .bg(if notice.is_error {
                                    p.bad.opacity(0.14)
                                } else {
                                    p.good.opacity(0.14)
                                })
                                .border_b_1()
                                .border_color(if notice.is_error { p.bad } else { p.good })
                                .child(
                                    div()
                                        .flex_1()
                                        .text_xs()
                                        .text_color(if notice.is_error { p.bad } else { p.text })
                                        .child(notice.text),
                                )
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .items_center()
                                        .when_some(notice.url, |row, url| {
                                            row.child(
                                                Button::new("git-notice-open-pr")
                                                    .xsmall()
                                                    .primary()
                                                    .label("Open PR ↗")
                                                    .on_click(move |_, _, _| {
                                                        let _ = files::open_url(&url);
                                                    }),
                                            )
                                        })
                                        .child(
                                            Button::new("git-notice-dismiss")
                                                .xsmall()
                                                .ghost()
                                                .label("Dismiss")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.git_notice = None;
                                                    cx.notify();
                                                })),
                                        ),
                                ),
                        )
                    })
                    // Sub-agent tracking strip + inspector drawer (shown only when sub-agents exist)
                    .when(!self.subagents.is_empty(), |col| {
                        col.child(self.render_subagent_strip(p, window, cx))
                    })
                    .when_some(
                        self.inspected_subagent
                            .as_deref()
                            .and_then(|id| self.subagents.iter().find(|s| s.id == id))
                            .cloned(),
                        |col, rec| col.child(self.render_subagent_inspector(&rec, p, cx)),
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
                        let images = pending.images.clone();
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
                                            this.pending_images.clear();
                                            this.attach_notice = None;
                                            this.dispatch_send(
                                                display_text.clone(),
                                                prompt_text.clone(),
                                                images.clone(),
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
                        let pid = self.selected_project.clone().unwrap_or_default();
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
                                            this.archive_chat(
                                                pid.clone(),
                                                sid.clone(),
                                                true,
                                                window,
                                                cx,
                                            );
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
                    // Keyboard shortcuts help panel (⌘/)
                    .when(self.show_shortcuts_help, |col| {
                        col.child(self.render_shortcuts_help(p, cx))
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
                    // Attached images preview strip & attachment warnings above composer
                    .when(
                        !pending_images_snapshot.is_empty() || self.attach_notice.is_some(),
                        |col| {
                            col.child(
                                v_flex()
                                    .flex_shrink_0()
                                    .px_4()
                                    .py_2()
                                    .gap_2()
                                    .bg(p.panel)
                                    .border_t_1()
                                    .border_color(p.line)
                                    .when_some(self.attach_notice.clone(), |strip, notice| {
                                        strip.child(
                                            h_flex()
                                                .items_center()
                                                .justify_between()
                                                .gap_2()
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .text_xs()
                                                        .text_color(p.warn)
                                                        .child(notice),
                                                )
                                                .child(
                                                    Button::new("dismiss-attach-notice")
                                                        .ghost()
                                                        .xsmall()
                                                        .label("×")
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.attach_notice = None;
                                                            cx.notify();
                                                        })),
                                                ),
                                        )
                                    })
                                    .when(!pending_images_snapshot.is_empty(), |strip| {
                                        let count = pending_images_snapshot.len();
                                        let share_paths = self
                                            .backend
                                            .app
                                            .settings
                                            .lock()
                                            .unwrap()
                                            .include_image_paths;
                                        strip.child(
                                            h_flex()
                                                .w_full()
                                                .flex_wrap()
                                                .gap_2()
                                                .items_center()
                                                .children(pending_images_snapshot.iter().map(
                                                    |attached| {
                                                        let img_id = attached.id;
                                                        let mut size_label =
                                                            attachments::format_bytes(
                                                                attached.attachment.size_bytes,
                                                            );
                                                        if share_paths
                                                            && attached
                                                                .attachment
                                                                .source_path
                                                                .is_some()
                                                        {
                                                            size_label.push_str(" · path");
                                                        }
                                                        h_flex()
                                                            .items_center()
                                                            .gap_2()
                                                            .p_1p5()
                                                            .rounded(px(8.0))
                                                            .bg(p.bg)
                                                            .border_1()
                                                            .border_color(p.line)
                                                            .when_some(
                                                                attached.preview.clone(),
                                                                |card, preview| {
                                                                    card.child(
                                                                        img(preview)
                                                                            .size(px(44.0))
                                                                            .object_fit(
                                                                                ObjectFit::Cover,
                                                                            )
                                                                            .rounded(px(5.0))
                                                                            .flex_shrink_0(),
                                                                    )
                                                                },
                                                            )
                                                            .child(
                                                                v_flex()
                                                                    .max_w(px(140.0))
                                                                    .min_w_0()
                                                                    .child(
                                                                        div()
                                                                            .text_xs()
                                                                            .font_weight(
                                                                                gpui_kit::FontWeight::MEDIUM,
                                                                            )
                                                                            .truncate()
                                                                            .child(
                                                                                attached
                                                                                    .attachment
                                                                                    .name
                                                                                    .clone(),
                                                                            ),
                                                                    )
                                                                    .child(
                                                                        div()
                                                                            .text_size(px(10.0))
                                                                            .text_color(p.muted)
                                                                            .child(size_label),
                                                                    ),
                                                            )
                                                            .child(
                                                                Button::new((
                                                                    "remove-pending-img",
                                                                    img_id,
                                                                ))
                                                                .ghost()
                                                                .xsmall()
                                                                .label("×")
                                                                .on_click(cx.listener(
                                                                    move |this, _, _, cx| {
                                                                        this.remove_pending_image(
                                                                            img_id, cx,
                                                                        );
                                                                    },
                                                                )),
                                                            )
                                                    },
                                                ))
                                                .when(count > 1, |row| {
                                                    row.child(
                                                        Button::new("clear-pending-imgs")
                                                            .ghost()
                                                            .xsmall()
                                                            .label("Clear all")
                                                            .on_click(cx.listener(
                                                                |this, _, _, cx| {
                                                                    this.pending_images.clear();
                                                                    this.attach_notice = None;
                                                                    cx.notify();
                                                                },
                                                            )),
                                                    )
                                                }),
                                        )
                                    }),
                            )
                        },
                    )
                    // Composer (unified T3 Code-style card: full-width textarea + bottom paperclip & Send bar)
                    .child({
                        let composer_val = self.composer.read(cx).value();
                        let line_count = composer_val.split('\n').count().clamp(2, 6) as f32;
                        let composer_h = px(line_count * 20.0 + 10.0);
                        let composer_focused = self
                            .composer
                            .read(cx)
                            .focus_handle(cx)
                            .is_focused(window);
                        div()
                            .w_full()
                            .flex_shrink_0()
                            .px_4()
                            .py_3()
                            .bg(p.panel)
                            .border_t_1()
                            .border_color(p.line)
                            .child(
                                v_flex()
                                    .w_full()
                                    .rounded(px(10.0))
                                    .bg(p.bg)
                                    .border_1()
                                    .border_color(if composer_focused {
                                        p.accent
                                    } else {
                                        p.line
                                    })
                                    .px_2p5()
                                    .pt_2()
                                    .pb_1p5()
                                    .gap_1()
                                    .child(
                                        div().w_full().min_w_0().child(
                                            Textarea::new(&self.composer)
                                                .appearance(false)
                                                .bordered(false)
                                                .h(composer_h)
                                                .on_paste(move |item, window, cx| {
                                                    let Some(view) = view_for_paste.upgrade()
                                                    else {
                                                        return false;
                                                    };
                                                    view.update(cx, |this, cx| {
                                                        this.attach_clipboard_item(
                                                            item, window, cx,
                                                        )
                                                    })
                                                }),
                                        ),
                                    )
                                    .child(
                                        h_flex()
                                            .w_full()
                                            .items_center()
                                            .justify_between()
                                            .gap_2()
                                            .child(
                                                Button::new("attach-image-btn")
                                                    .ghost()
                                                    .xsmall()
                                                    .icon(IconName::Paperclip)
                                                    .tooltip(
                                                        "Attach image (⌘⇧I, or drag & drop / ⌘V)",
                                                    )
                                                    .on_click(cx.listener(
                                                        |this, _, window, cx| {
                                                            this.choose_images(window, cx);
                                                        },
                                                    )),
                                            )
                                            .child(if self.busy {
                                                Button::new("stop-btn")
                                                    .danger()
                                                    .small()
                                                    .label("Stop")
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.stop_current(cx);
                                                    }))
                                            } else {
                                                Button::new("send-btn")
                                                    .primary()
                                                    .small()
                                                    .label("Send")
                                                    .disabled(!can_send)
                                                    .on_click(cx.listener(
                                                        |this, _, window, cx| {
                                                            this.send_current(window, cx);
                                                        },
                                                    ))
                                            }),
                                    ),
                            )
                    }),
            )
            .when(self.show_projects_popout, |root| {
                root.child(self.render_projects_popout(p, cx))
            })
            .when(self.show_new_chat_modal, |root| {
                root.child(self.render_new_chat_modal(p, cx))
            })
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

fn format_working_compact(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn project_monogram(name: &str) -> String {
    let words: Vec<&str> = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let chars: String = if words.len() >= 2 {
        let first = words[0].chars().next();
        let second = words[1].chars().next();
        first.into_iter().chain(second).collect()
    } else {
        name.chars().filter(|c| c.is_alphanumeric()).take(2).collect()
    };
    if chars.is_empty() {
        "PR".into()
    } else {
        chars.to_uppercase()
    }
}

fn project_badge_colors(id: &str, is_dark: bool) -> (Hsla, Hsla) {
    let palette: &[(u32, u32)] = if is_dark {
        &[
            (0x2f3542, 0xcbd5e1), // slate
            (0x3b2c5e, 0xd8b4fe), // purple
            (0x164e63, 0x67e8f9), // cyan
            (0x1e4633, 0x86efac), // emerald
            (0x523718, 0xfcd34d), // amber
            (0x522238, 0xfda4af), // rose
            (0x1e3a5f, 0x93c5fd), // blue
            (0x364420, 0xbef264), // lime
        ]
    } else {
        &[
            (0xe2e8f0, 0x334155), // slate
            (0xf3e8ff, 0x6b21a8), // purple
            (0xcffafe, 0x155e75), // cyan
            (0xdcfce7, 0x166534), // emerald
            (0xfef3c7, 0x92400e), // amber
            (0xffe4e6, 0x9f1239), // rose
            (0xdbeafe, 0x1e40af), // blue
            (0xecfccb, 0x3f6212), // lime
        ]
    };
    let mut hash: u64 = 14695981039346656037;
    for b in id.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(1099511628211);
    }
    let (bg, fg) = palette[(hash as usize) % palette.len()];
    (gpui_kit::rgb(bg).into(), gpui_kit::rgb(fg).into())
}

fn parse_session_timestamp(val: &Value) -> Option<u64> {
    for key in ["updatedAt", "updated_at", "updated", "createdAt", "created_at"] {
        let v = &val[key];
        if let Some(n) = v.as_u64() {
            return Some(if n > 10_000_000_000 { n / 1000 } else { n });
        }
        if let Some(s) = v.as_str()
            && let Some(ts) = parse_rfc3339_secs(s)
        {
            return Some(ts);
        }
    }
    None
}

fn parse_rfc3339_secs(s: &str) -> Option<u64> {
    let bytes = s.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    let year: i64 = s[0..4].parse().ok()?;
    let month: i64 = s[5..7].parse().ok()?;
    let day: i64 = s[8..10].parse().ok()?;
    let hour: i64 = s[11..13].parse().ok()?;
    let min: i64 = s[14..16].parse().ok()?;
    let sec: i64 = s[17..19].parse().ok()?;
    // Howard Hinnant's civil-from-days inverse (UTC).
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hour * 3600 + min * 60 + sec;
    u64::try_from(secs).ok()
}

fn format_relative_time(epoch_secs: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let diff = now.saturating_sub(epoch_secs);
    if diff < 60 {
        "now".into()
    } else if diff < 3600 {
        format!("{}m", diff / 60)
    } else if diff < 86_400 {
        format!("{}h", diff / 3600)
    } else if diff < 86_400 * 30 {
        format!("{}d", diff / 86_400)
    } else {
        format!("{}mo", diff / (86_400 * 30))
    }
}
