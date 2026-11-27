//! Saved conversations (`Chat`), compaction checkpoints, and branching (`fork`).
//!
//! `Chat.messages` always preserves the full user-visible transcript in OpenAI message format
//! (without the system prompt, which is built fresh per request). When context compaction runs,
//! a [`Compaction`] checkpoint is stored alongside the messages so the UI keeps the full history
//! while subsequent model requests start from the summary plus newer messages.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A saved conversation compaction checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compaction {
    /// Messages strictly before this index in [`Chat::messages`] are replaced by `summary`
    /// when building requests for the model.
    pub before_index: usize,
    /// Structured summary covering earlier turns.
    pub summary: String,
    /// Seconds since 1970 when compaction ran.
    pub created: u64,
}

/// One conversation with the agent about a project, as the model sees it (OpenAI messages,
/// without the system prompt, which is made fresh for each request).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chat {
    pub id: String,
    pub project: String,
    pub title: String,
    /// Seconds since 1970.
    pub created: u64,
    pub updated: u64,
    pub messages: Vec<Value>,
    /// Latest compaction checkpoint, if this chat was compacted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<Compaction>,
    /// Parent chat id when forked from an earlier chat (Pi-style branching).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Message index in the parent chat where this branch diverged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_at: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSummary {
    pub id: String,
    pub title: String,
    pub updated: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
}

/// Filesystem store for chats under `<data_dir>/chats/<project>/<id>.json`.
#[derive(Debug, Clone)]
pub struct ChatStore {
    root: PathBuf,
}

impl ChatStore {
    /// Creates a store rooted at `data_dir` (chats are stored in `data_dir/chats`).
    pub fn in_data_dir(data_dir: impl Into<PathBuf>) -> Self {
        Self { root: data_dir.into().join("chats") }
    }

    /// Creates a store whose chats live directly inside `chats_dir/<project>/`.
    pub fn at(chats_dir: impl Into<PathBuf>) -> Self {
        Self { root: chats_dir.into() }
    }

    fn folder(&self, project: &str) -> anyhow::Result<PathBuf> {
        anyhow::ensure!(valid_id(project), "bad project id");
        Ok(self.root.join(project))
    }

    pub fn load(&self, project: &str, id: &str) -> anyhow::Result<Chat> {
        anyhow::ensure!(valid_id(id), "bad chat id");
        let path = self.folder(project)?.join(format!("{id}.json"));
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        Ok(serde_json::from_str(&raw)?)
    }

    pub fn save(&self, chat: &mut Chat) -> anyhow::Result<()> {
        anyhow::ensure!(valid_id(&chat.id), "bad chat id");
        chat.updated = now();
        if chat.title == "New chat"
            && let Some(first) = chat
                .messages
                .iter()
                .find(|m| m["role"] == "user")
                .and_then(|m| user_message_title_source(&m["content"]))
        {
            chat.title = title(&first);
        }
        let dir = self.folder(&chat.project)?;
        ensure_private_dir(&dir)?;
        write_private(&dir.join(format!("{}.json", chat.id)), serde_json::to_string(chat)?.as_bytes())?;
        Ok(())
    }

    /// A project's chats, most recently updated first.
    pub fn list(&self, project: &str) -> Vec<ChatSummary> {
        let Ok(dir) = self.folder(project) else {
            return Vec::new();
        };
        let mut chats: Vec<ChatSummary> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| serde_json::from_str::<Chat>(&std::fs::read_to_string(e.path()).ok()?).ok())
            .map(|c| ChatSummary {
                id: c.id,
                title: c.title,
                updated: c.updated,
                parent_id: c.parent_id,
            })
            .collect();
        chats.sort_by_key(|c| std::cmp::Reverse(c.updated));
        chats
    }

    pub fn delete(&self, project: &str, id: &str) -> anyhow::Result<()> {
        anyhow::ensure!(valid_id(id), "bad chat id");
        std::fs::remove_file(self.folder(project)?.join(format!("{id}.json")))?;
        Ok(())
    }

    /// Removes all of a project's chats (when the project is deleted).
    pub fn delete_all(&self, project: &str) {
        if let Ok(dir) = self.folder(project) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// Creates and saves a branched copy of `id` containing messages `..keep_count`.
    pub fn fork(&self, project: &str, id: &str, keep_count: usize) -> anyhow::Result<Chat> {
        let source = self.load(project, id)?;
        let mut forked = source.fork(keep_count);
        self.save(&mut forked)?;
        Ok(forked)
    }
}

impl Chat {
    pub fn new(project: &str) -> Self {
        let mut id = [0u8; 8];
        getrandom::fill(&mut id).expect("system randomness");
        let now = now();
        Self {
            id: hex::encode(id),
            project: project.to_string(),
            title: "New chat".into(),
            created: now,
            updated: now,
            messages: Vec::new(),
            compaction: None,
            parent_id: None,
            forked_at: None,
        }
    }

    /// Creates a new chat branching from `self` with the first `keep_count` messages.
    /// If `self` had a compaction checkpoint strictly within `keep_count`, it is preserved.
    pub fn fork(&self, keep_count: usize) -> Self {
        let keep = keep_count.min(self.messages.len());
        let mut forked = Self::new(&self.project);
        forked.messages = self.messages[..keep].to_vec();
        forked.parent_id = Some(self.id.clone());
        forked.forked_at = Some(keep);
        if let Some(comp) = &self.compaction
            && comp.before_index <= keep
        {
            forked.compaction = Some(comp.clone());
        }
        if self.title != "New chat" {
            forked.title = format!("{} (branch)", self.title.trim_end_matches(" (branch)"));
        }
        forked
    }

    /// Records a new compaction checkpoint replacing messages before `before_index`.
    pub fn set_compaction(&mut self, before_index: usize, summary: String) {
        self.compaction = Some(Compaction {
            before_index: before_index.min(self.messages.len()),
            summary,
            created: now(),
        });
    }

    /// Convenience wrappers matching the legacy `Chat::load`/`save`/`list`/`delete`/`delete_all` API
    /// when given a data directory.
    pub fn load_in(data_dir: &Path, project: &str, id: &str) -> anyhow::Result<Self> {
        ChatStore::in_data_dir(data_dir).load(project, id)
    }

    pub fn save_in(&mut self, data_dir: &Path) -> anyhow::Result<()> {
        ChatStore::in_data_dir(data_dir).save(self)
    }

    pub fn list_in(data_dir: &Path, project: &str) -> Vec<ChatSummary> {
        ChatStore::in_data_dir(data_dir).list(project)
    }

    pub fn delete_in(data_dir: &Path, project: &str, id: &str) -> anyhow::Result<()> {
        ChatStore::in_data_dir(data_dir).delete(project, id)
    }

    pub fn delete_all_in(data_dir: &Path, project: &str) {
        ChatStore::in_data_dir(data_dir).delete_all(project);
    }
}

pub(crate) fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&tmp, bytes)?;
    }
    std::fs::rename(tmp, path)?;
    Ok(())
}

/// Extracts the human-readable title seed from a user message's `content` (string or multimodal array).
pub fn user_message_title_source(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => (!s.trim().is_empty()).then(|| s.clone()),
        Value::Array(parts) => {
            let text: String = parts
                .iter()
                .filter(|p| {
                    !p.get("superviser_meta").and_then(Value::as_bool).unwrap_or(false)
                        && matches!(p.get("type").and_then(Value::as_str), Some("text" | "input_text"))
                })
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            if !text.is_empty() {
                return Some(text);
            }
            let image_count = parts
                .iter()
                .filter(|p| matches!(p.get("type").and_then(Value::as_str), Some("image_url" | "input_image" | "image")))
                .count();
            if image_count == 1 {
                Some("Image".into())
            } else if image_count > 1 {
                Some(format!("{image_count} images"))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// A chat's title: the start of its first message, on one line.
fn title(message: &str) -> String {
    let line = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= 60 {
        return line;
    }
    let cut: String = line.chars().take(57).collect();
    format!("{}…", cut.rsplit_once(' ').map_or(cut.as_str(), |(start, _)| start))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chats_are_saved_listed_forked_and_titled() {
        let root = std::env::temp_dir().join(format!("entropy-chats-{}", std::process::id()));
        let store = ChatStore::in_data_dir(&root);
        let mut chat = Chat::new("ab12");
        chat.messages.push(json!({"role": "user", "content": "Fix the   failing test in\nsrc/lib.rs please, it has been red since the refactor last week"}));
        chat.messages.push(json!({"role": "assistant", "content": "Looking into it."}));
        chat.messages.push(json!({"role": "user", "content": "Try a different approach"}));
        store.save(&mut chat).unwrap();
        assert_eq!(chat.title, "Fix the failing test in src/lib.rs please, it has been…");

        // Fork keeps messages up to the branch point and tracks parent.
        let forked = store.fork("ab12", &chat.id, 2).unwrap();
        assert_eq!(forked.messages.len(), 2);
        assert_eq!(forked.parent_id.as_deref(), Some(chat.id.as_str()));
        assert_eq!(forked.forked_at, Some(2));
        assert!(forked.title.ends_with("(branch)"));

        assert_eq!(store.list("ab12").len(), 2);
        assert_eq!(store.load("ab12", &chat.id).unwrap().messages.len(), 3);
        assert!(store.load("ab12", "../x").is_err() && store.load("../x", "ab").is_err());

        store.delete("ab12", &forked.id).unwrap();
        assert_eq!(store.list("ab12").len(), 1);
        store.delete_all("ab12");
        assert!(store.list("ab12").is_empty());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn multimodal_messages_derive_chat_titles() {
        let with_text = json!([
            {"type": "text", "text": "Why is this button misaligned?"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
        ]);
        assert_eq!(
            user_message_title_source(&with_text).as_deref(),
            Some("Why is this button misaligned?")
        );
        let one_image = json!([
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
        ]);
        assert_eq!(user_message_title_source(&one_image).as_deref(), Some("Image"));
    }
}
