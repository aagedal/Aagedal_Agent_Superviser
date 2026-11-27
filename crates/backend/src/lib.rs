//! Shared UI-agnostic application runtime for Agent Superviser.
//!
//! Both the Tauri menu bar app (`app/`) and the native GPUI app (`app-gpui/`) drive
//! the same `App`, `Engines` (OpenCode ACP), `Chats` (built-in agent), downloads,
//! model servers, guard proxy, and file-opening security rules through [`events::EventBus`].

pub mod acp;
pub mod agent;
pub mod attachments;
pub mod events;
pub mod explain;
pub mod files;
pub mod git;
pub mod skills;
pub mod state;
pub mod subagents;

pub use attachments::ImageAttachment;
pub use events::{AcpEvent, AppEvent, ChatEvent, DownloadEvent, EventBus};
pub use state::{
    App, DEFAULT_GUARD_PORT, JUDGE_DOWNLOAD, JUDGE_NAME, JudgeStatus, LoadState, LoadedModel,
    ReasoningLevel, Room, ServerStatus, Settings,
};
pub use subagents::{SubagentRecord, SubagentState, SubagentStep};
