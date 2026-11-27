//! **Entropy** — a lean, standalone coding agent harness tuned for local models.
//!
//! Zero dependencies on UI frameworks, model servers, or OS-specific sandboxes:
//! - [`prompt`]: ~250-token system prompt with `AGENTS.md` / `CLAUDE.md` and `.entropy/SYSTEM.md` overrides.
//! - [`tools`] & [`edit`]: 6 focused tools (`read`, `glob`, `grep`, `write`, `edit`, `bash`) with
//!   forgiving multi-strategy edit matching, atomic multi-hunk edits, closest-snippet hints,
//!   concise unified diffs, and pluggable [`tools::CommandRunner`] execution.
//! - [`context`]: Live token estimation, stale tool-output pruning, and structured conversation compaction.
//! - [`chat`]: Persistent JSON transcripts with non-destructive compaction checkpoints and chat branching (`fork`).
//! - [`engine`]: Streaming OpenAI-compatible turn loop with mid-turn steering (`steer`),
//!   interactive tool approvals, and pluggable read-only sub-agents.

pub mod chat;
pub mod context;
pub mod edit;
pub mod engine;
pub mod prompt;
pub mod tools;

pub use chat::{Chat, ChatStore, ChatSummary, Compaction, user_message_title_source};
pub use context::ContextBudget;
pub use edit::{EditOutcome, EditSpec, MatchStrategy, apply_edit, apply_multi_edit, unified_diff};
pub use engine::{
    ApprovalPolicy, EventSink, HarnessEvent, MAX_STEPS, ModelEndpoint, SessionManager, StreamReply,
    SubagentHandler, TokenUsage, ToolCall, TurnConfig, compact_chat, run_turn,
};
pub use prompt::PromptConfig;
pub use tools::{CommandOutput, CommandRunner, Kind, PathMapper, Workspace, WorkspaceFolder, clip, kind, run_process};
