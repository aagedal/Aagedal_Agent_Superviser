//! UI-agnostic event bus bridging Tokio background tasks to Tauri webviews or GPUI views.
//!
//! Uses unbounded per-subscriber channels rather than a fixed-capacity broadcast ring so
//! bursts such as OpenCode ACP `session/load` replays never drop updates or permission asks.

use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::Value;
use superviser_guard::CallEvent;
use tokio::sync::mpsc;

use crate::state::ServerStatus;
use crate::subagents::SubagentRecord;

/// What a chat window hears from OpenCode over ACP.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AcpEvent {
    /// A `session/update` from OpenCode, as it sent it.
    Update { project: String, session: String, update: Value },
    /// OpenCode asks before a call; answer with `Engine::answer`.
    Permission { project: String, session: String, request: String, tool_call: Value, options: Value },
    /// A prompt started or ended.
    State { project: String, session: String, busy: bool, stop_reason: Option<String>, error: Option<String> },
    /// OpenCode is starting slowly for the project: why.
    Starting { project: String, text: String },
}

/// What a chat window hears from the built-in harness.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatEvent {
    /// A message was added to the chat.
    Message { chat: String, message: Value },
    /// The reply so far.
    Delta { chat: String, content: String, reasoning: String },
    /// A call waits for the user's yes or no.
    Approval { chat: String, call: String, name: String, arguments: String },
    /// Live context window usage (tokens used out of model context size).
    Usage { chat: String, used: u64, size: u64 },
    /// Informational status notice (e.g. automatic context compaction).
    Notice { chat: String, text: String },
    /// The turn started or ended (with what went wrong, if something did).
    State { chat: String, busy: bool, error: Option<String> },
}

/// Progress or completion of a model or judge download.
#[derive(Debug, Clone, Serialize)]
pub struct DownloadEvent {
    pub id: String,
    /// What's downloading, and the memory it takes while running: a window opened
    /// mid-download learns them from active downloads or the next event.
    pub name: String,
    pub needs: Option<u64>,
    pub done: u64,
    pub total: u64,
    pub finished: bool,
    pub error: Option<String>,
}

/// Every event emitted by the shared backend runtime.
#[derive(Debug, Clone)]
pub enum AppEvent {
    Status(ServerStatus),
    /// Intermediate model-loading progress update (doesn't require rebuilding the macOS tray menu).
    LoadProgress { model: String, name: String, progress: f32, stage: String },
    GuardCall(CallEvent),
    Discovery,
    Projects,
    Judge,
    Reasoning,
    Download(DownloadEvent),
    Acp(AcpEvent),
    Chat(ChatEvent),
    Subagent(SubagentRecord),
}

/// Lossless fan-out event bus backed by per-subscriber unbounded mpsc channels.
#[derive(Clone, Default)]
pub struct EventBus {
    subscribers: Arc<Mutex<Vec<mpsc::UnboundedSender<AppEvent>>>>,
}

impl EventBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a receiver for all future events. Dropping the receiver unsubscribes it automatically.
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<AppEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.subscribers.lock().unwrap().push(tx);
        rx
    }

    /// Delivers `event` to every live subscriber, pruning dropped ones.
    pub fn emit(&self, event: AppEvent) {
        let mut subs = self.subscribers.lock().unwrap();
        subs.retain(|tx| tx.send(event.clone()).is_ok());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bus_fans_out_and_prunes_closed_subscribers() {
        let bus = EventBus::new();
        let mut rx1 = bus.subscribe();
        let rx2 = bus.subscribe();
        drop(rx2);

        bus.emit(AppEvent::Discovery);
        bus.emit(AppEvent::Projects);

        assert!(matches!(rx1.try_recv(), Ok(AppEvent::Discovery)));
        assert!(matches!(rx1.try_recv(), Ok(AppEvent::Projects)));
        assert_eq!(bus.subscribers.lock().unwrap().len(), 1);
    }
}
