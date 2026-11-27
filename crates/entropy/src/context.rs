//! Pi-inspired context budget management for local models:
//!
//! 1. **Token estimation** (used alongside real `usage.prompt_tokens` reported by `llama-server`).
//! 2. **Stage 1 — Stale tool-output pruning (zero LLM calls):** Once context crosses ~65% of the
//!    model's window, older large `role: "tool"` outputs (leaving the most recent tool results
//!    intact) are replaced in the outgoing request with compact stubs noting what was pruned.
//!    The saved `Chat.messages` on disk remain untouched for the UI.
//! 3. **Stage 2 — Structured conversation compaction:** When context crosses ~80% (or on manual
//!    `/compact`), older turns are summarized into a [`Compaction`] checkpoint preserving goals,
//!    modified files, decisions, and open tasks.

use serde_json::{Value, json};

use crate::chat::{Chat, Compaction};

/// Thresholds controlling pruning and automatic compaction.
#[derive(Debug, Clone, Copy)]
pub struct ContextBudget {
    /// Model context window in tokens.
    pub context_window: u64,
    /// Fraction of `context_window` at which stale tool outputs begin being pruned (e.g. 0.65).
    pub prune_ratio: f32,
    /// Fraction of `context_window` at which automatic compaction triggers (e.g. 0.80).
    pub compact_ratio: f32,
    /// Minimum number of most-recent `role: "tool"` messages left verbatim during pruning.
    pub keep_recent_tools: usize,
    /// Minimum number of trailing messages kept un-compacted during auto-compaction.
    pub keep_recent_messages: usize,
}

impl ContextBudget {
    pub fn for_window(context_window: u64) -> Self {
        Self {
            context_window: context_window.max(4_096),
            prune_ratio: 0.65,
            compact_ratio: 0.80,
            keep_recent_tools: 4,
            keep_recent_messages: 6,
        }
    }

    pub fn prune_threshold(&self) -> u64 {
        ((self.context_window as f64) * (self.prune_ratio as f64)) as u64
    }

    pub fn compact_threshold(&self) -> u64 {
        ((self.context_window as f64) * (self.compact_ratio as f64)) as u64
    }
}

/// Fast local token estimate (conservative ~3.5 chars/token + overhead for JSON/images).
pub fn estimate_value_tokens(value: &Value) -> u64 {
    match value {
        Value::Null => 1,
        Value::Bool(_) => 1,
        Value::Number(_) => 2,
        Value::String(s) => {
            // Base64 image data URLs consume vision tokens, not raw base64 chars.
            if s.starts_with("data:image/") {
                1_024
            } else {
                (s.len() as u64).div_ceil(3).max(1)
            }
        }
        Value::Array(arr) => arr.iter().map(estimate_value_tokens).sum::<u64>() + 2,
        Value::Object(map) => {
            map.iter()
                .map(|(k, v)| (k.len() as u64).div_ceil(4) + estimate_value_tokens(v) + 1)
                .sum::<u64>()
                + 4
        }
    }
}

/// Estimates total prompt tokens for a system message, active chat messages, and tool schemas.
pub fn estimate_request_tokens(system: &Value, messages: &[Value], tools: &[Value]) -> u64 {
    let sys = estimate_value_tokens(system);
    let msgs: u64 = messages.iter().map(estimate_value_tokens).sum();
    let tls: u64 = tools.iter().map(estimate_value_tokens).sum();
    sys + msgs + tls
}

/// Strips internal harness/UI metadata from a message before sending it to the model.
pub fn for_model(message: &Value) -> Value {
    let mut message = message.clone();
    if let Some(object) = message.as_object_mut() {
        object.remove("reasoning");
        if let Some(parts) = object.get_mut("content").and_then(Value::as_array_mut) {
            for part in parts {
                if let Some(part_obj) = part.as_object_mut() {
                    part_obj.remove("superviser_meta");
                }
            }
        }
    }
    message
}

/// Builds the active message list sent to the model (after the system prompt), honoring any
/// saved [`Compaction`] checkpoint on `chat`.
pub fn active_messages(chat: &Chat) -> Vec<Value> {
    match &chat.compaction {
        Some(Compaction { before_index, summary, .. }) => {
            let start = (*before_index).min(chat.messages.len());
            let mut out = Vec::with_capacity(1 + chat.messages.len().saturating_sub(start));
            out.push(json!({
                "role": "user",
                "content": format!(
                    "[Conversation summary of earlier turns — continue from here]\n\n{summary}"
                )
            }));
            out.extend(chat.messages[start..].iter().map(for_model));
            out
        }
        None => chat.messages.iter().map(for_model).collect(),
    }
}

/// Stage 1: Replaces large stale `role: "tool"` outputs (older than `budget.keep_recent_tools`)
/// with compact stubs when the estimated prompt exceeds `budget.prune_threshold()`.
/// Returns the number of tool messages pruned.
pub fn prune_stale_tool_outputs(
    system: &Value,
    messages: &mut [Value],
    tools: &[Value],
    budget: &ContextBudget,
) -> usize {
    const MIN_PRUNABLE_CHARS: usize = 800;
    const STUB_PREVIEW_CHARS: usize = 160;

    if estimate_request_tokens(system, messages, tools) < budget.prune_threshold() {
        return 0;
    }

    let tool_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter_map(|(i, m)| (m["role"].as_str() == Some("tool")).then_some(i))
        .collect();
    let cutoff = tool_indices.len().saturating_sub(budget.keep_recent_tools);
    let mut pruned = 0;

    for &idx in &tool_indices[..cutoff] {
        let Some(content) = messages[idx]["content"].as_str() else {
            continue;
        };
        if content.len() < MIN_PRUNABLE_CHARS {
            continue;
        }
        let preview: String = content
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(STUB_PREVIEW_CHARS)
            .collect();
        let stub = format!(
            "[Earlier tool output pruned to save context ({} chars). First line: {:?}. Re-run tool if needed.]",
            content.len(),
            preview
        );
        messages[idx]["content"] = Value::String(stub);
        pruned += 1;

        if estimate_request_tokens(system, messages, tools) < budget.prune_threshold() {
            break;
        }
    }
    pruned
}

/// Chooses a clean compaction cut index in `chat.messages` that keeps at least
/// `keep_recent_messages` trailing messages and never splits an assistant `tool_calls`
/// message from its corresponding `role: "tool"` replies.
pub fn compaction_split_index(chat: &Chat, keep_recent_messages: usize) -> Option<usize> {
    let start = chat.compaction.as_ref().map_or(0, |c| c.before_index);
    let active_len = chat.messages.len().saturating_sub(start);
    if active_len <= keep_recent_messages + 2 {
        return None;
    }
    let mut split = chat.messages.len().saturating_sub(keep_recent_messages);
    // Never start the retained tail on a `tool` message (which would orphan its `tool_call_id`).
    while split > start && chat.messages[split]["role"].as_str() == Some("tool") {
        split -= 1;
    }
    // Also don't split right onto an assistant message with tool calls without pulling back if needed,
    // or ensure we actually compact at least 2 messages.
    if split <= start + 1 {
        return None;
    }
    Some(split)
}

/// Builds the OpenAI chat-completions messages for summarizing `chat.messages[..split_index]`.
pub fn build_compaction_prompt(chat: &Chat, split_index: usize) -> Vec<Value> {
    let start = chat.compaction.as_ref().map_or(0, |c| c.before_index);
    let mut transcript = String::new();
    if let Some(prev) = &chat.compaction {
        transcript.push_str("## Previous summary\n");
        transcript.push_str(&prev.summary);
        transcript.push_str("\n\n## Subsequent turns to fold in\n");
    }
    for msg in &chat.messages[start..split_index.min(chat.messages.len())] {
        let role = msg["role"].as_str().unwrap_or("unknown");
        let content = match &msg["content"] {
            Value::String(s) => s.clone(),
            Value::Array(parts) => parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        let snippet = crate::tools::clip(content.trim(), 1_200);
        if !snippet.is_empty() {
            transcript.push_str(&format!("[{role}]: {snippet}\n"));
        }
        if let Some(calls) = msg["tool_calls"].as_array() {
            for call in calls {
                let name = call["function"]["name"].as_str().unwrap_or("tool");
                let args = call["function"]["arguments"].as_str().unwrap_or("");
                transcript.push_str(&format!("[tool_call {name}]: {}\n", crate::tools::clip(args, 240)));
            }
        }
    }

    vec![
        json!({
            "role": "system",
            "content": "Summarize this coding session tightly so the coding agent can continue seamlessly. \
                        Keep ONLY high-signal facts in short bullet points:\n\
                        - Goal & user constraints\n\
                        - Key findings (exact file paths, symbols, errors)\n\
                        - Changes already made\n\
                        - Remaining work / next steps\n\
                        Be concise (under 400 words). Do not invent facts."
        }),
        json!({
            "role": "user",
            "content": crate::tools::clip(&transcript, 24_000)
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prunes_only_older_large_tool_outputs() {
        let system = json!({"role": "system", "content": "sys"});
        let big = "line\n".repeat(4_000); // ~20k chars
        let mut msgs = vec![
            json!({"role": "user", "content": "inspect"}),
            json!({"role": "tool", "tool_call_id": "1", "content": big}),
            json!({"role": "tool", "tool_call_id": "2", "content": big}),
            json!({"role": "tool", "tool_call_id": "3", "content": "recent output"}),
        ];
        let budget = ContextBudget {
            context_window: 8_000,
            prune_ratio: 0.50,
            compact_ratio: 0.80,
            keep_recent_tools: 1,
            keep_recent_messages: 4,
        };
        let pruned = prune_stale_tool_outputs(&system, &mut msgs, &[], &budget);
        assert!(pruned >= 1);
        assert!(msgs[1]["content"].as_str().unwrap().contains("pruned to save context"));
        assert_eq!(msgs[3]["content"].as_str().unwrap(), "recent output");
    }

    #[test]
    fn compaction_split_never_orphans_tool_messages() {
        let mut chat = Chat::new("p1");
        chat.messages = vec![
            json!({"role": "user", "content": "1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "2"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "c1"}]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "ok"}),
            json!({"role": "assistant", "content": "done"}),
            json!({"role": "user", "content": "3"}),
            json!({"role": "assistant", "content": "a3"}),
        ];
        let split = compaction_split_index(&chat, 4).unwrap();
        assert_ne!(chat.messages[split]["role"].as_str(), Some("tool"));
    }

    #[test]
    fn active_messages_starts_from_compaction_summary() {
        let mut chat = Chat::new("p1");
        chat.messages = vec![
            json!({"role": "user", "content": "old"}),
            json!({"role": "assistant", "content": "old reply"}),
            json!({"role": "user", "content": "new question"}),
        ];
        chat.set_compaction(2, "Fixed bug in src/main.rs".into());
        let active = active_messages(&chat);
        assert_eq!(active.len(), 2);
        assert!(active[0]["content"].as_str().unwrap().contains("Fixed bug in src/main.rs"));
        assert_eq!(active[1]["content"].as_str().unwrap(), "new question");
    }
}
