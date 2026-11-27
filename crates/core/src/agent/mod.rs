//! The built-in coding agent: its tools, its system prompt and its saved chats. The loop
//! that talks to the model (through the guard) lives in the app.

pub mod tools;

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::paths::{Paths, write_private};
use crate::project::{Folder, Project};

/// Characters of a project's instructions file put in the prompt.
const MAX_INSTRUCTIONS: usize = 8000;
/// Files a project can leave instructions for agents in, first found wins.
const INSTRUCTION_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md"];

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
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatSummary {
    pub id: String,
    pub title: String,
    pub updated: u64,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn folder(paths: &Paths, project: &str) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(valid_id(project), "bad project id");
    Ok(paths.data.join("chats").join(project))
}

impl Chat {
    pub fn new(project: &str) -> Self {
        let mut id = [0u8; 8];
        getrandom::fill(&mut id).expect("system randomness");
        let now = now();
        Self { id: hex::encode(id), project: project.to_string(), title: "New chat".into(), created: now, updated: now, messages: Vec::new() }
    }

    pub fn load(paths: &Paths, project: &str, id: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(valid_id(id), "bad chat id");
        let path = folder(paths, project)?.join(format!("{id}.json"));
        Ok(serde_json::from_str(&std::fs::read_to_string(&path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?)?)
    }

    pub fn save(&mut self, paths: &Paths) -> anyhow::Result<()> {
        anyhow::ensure!(valid_id(&self.id), "bad chat id");
        self.updated = now();
        if self.title == "New chat"
            && let Some(first) = self
                .messages
                .iter()
                .find(|m| m["role"] == "user")
                .and_then(|m| user_message_title_source(&m["content"]))
        {
            self.title = title(&first);
        }
        let dir = folder(paths, &self.project)?;
        crate::paths::private_dir(&dir)?;
        write_private(&dir.join(format!("{}.json", self.id)), serde_json::to_string(self)?.as_bytes())?;
        Ok(())
    }

    /// A project's chats, most recently updated first.
    pub fn list(paths: &Paths, project: &str) -> Vec<ChatSummary> {
        let Ok(dir) = folder(paths, project) else { return Vec::new() };
        let mut chats: Vec<ChatSummary> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| serde_json::from_str::<Chat>(&std::fs::read_to_string(e.path()).ok()?).ok())
            .map(|c| ChatSummary { id: c.id, title: c.title, updated: c.updated })
            .collect();
        chats.sort_by_key(|c| std::cmp::Reverse(c.updated));
        chats
    }

    pub fn delete(paths: &Paths, project: &str, id: &str) -> anyhow::Result<()> {
        anyhow::ensure!(valid_id(id), "bad chat id");
        std::fs::remove_file(folder(paths, project)?.join(format!("{id}.json")))?;
        Ok(())
    }

    /// Removes all of a project's chats (when the project goes).
    pub fn delete_all(paths: &Paths, project: &str) {
        if let Ok(dir) = folder(paths, project) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Extracts the human-readable title seed from a user message's `content` (string or multimodal array).
fn user_message_title_source(content: &Value) -> Option<String> {
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
    // At a word's end when there is one.
    format!("{}…", cut.rsplit_once(' ').map_or(cut.as_str(), |(start, _)| start))
}

/// The system prompt for a project: who the agent is, where it works and under which rules,
/// and the project's own instructions file if it has one.
pub fn system_prompt(project: &Project, main: &std::path::Path, related: &[Folder], commands: bool, edits: bool) -> String {
    let mut prompt = format!(
        "You are a coding agent working on the project \"{name}\" on the user's Mac, through Agent Superviser. \
         Work in small, verifiable steps: look at the code before changing it, make focused changes, and check them (build, test, run) when you can. \
         Keep replies short and plain; say what you changed and what you couldn't do.\n\n\
         Main folder: {main} (relative paths and commands start here).\n",
        name = project.name,
        main = main.display(),
    );
    for folder in related {
        prompt.push_str(&format!("Also {}: {}\n", if folder.writable { "yours to change" } else { "read-only" }, folder.path.display()));
    }
    prompt.push_str("\nTools: `read`, `glob` and `grep` for looking around");
    if edits {
        prompt.push_str("; `edit` (exact text replacement; read the file first) and `write` for changes");
    }
    if commands {
        prompt.push_str("; `bash` for commands (zsh on macOS)");
    }
    prompt.push_str(".\n");
    if !edits {
        prompt.push_str("This project doesn't let you change files: suggest changes instead.\n");
    }
    prompt.push_str(&format!(
        "\nRules you work under: commands run in a sandbox that can write only in the project's writable folders and temporary folders, \
         and can't read credentials or other private files. Internet is {}. A guard checks every tool call before it runs; \
         when it blocks one you get a note saying why. Don't try to get around a block: tell the user what you wanted to do and why.\n",
        if project.network == crate::sandbox::Network::Internet { "on" } else { "off (only this machine)" }
    ));
    for name in INSTRUCTION_FILES {
        if let Ok(text) = std::fs::read_to_string(main.join(name)) {
            prompt.push_str(&format!("\nThe project's {name}:\n\n{}\n", tools::clip(text.trim(), MAX_INSTRUCTIONS)));
            break;
        }
    }
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chats_are_saved_listed_and_titled() {
        let root = std::env::temp_dir().join(format!("superviser-chats-{}", std::process::id()));
        let paths = Paths::at(&root);
        let mut chat = Chat::new("ab12");
        chat.messages.push(serde_json::json!({"role": "user", "content": "Fix the   failing test in\nsrc/lib.rs please, it has been red since the refactor last week"}));
        chat.save(&paths).unwrap();
        assert_eq!(chat.title, "Fix the failing test in src/lib.rs please, it has been…");
        let mut other = Chat::new("ab12");
        other.save(&paths).unwrap();
        assert_eq!(Chat::list(&paths, "ab12").len(), 2);
        assert_eq!(Chat::load(&paths, "ab12", &chat.id).unwrap().messages.len(), 1);
        assert!(Chat::load(&paths, "ab12", "../x").is_err() && Chat::load(&paths, "../x", "ab").is_err());
        Chat::delete(&paths, "ab12", &other.id).unwrap();
        assert_eq!(Chat::list(&paths, "ab12").len(), 1);
        Chat::delete_all(&paths, "ab12");
        assert!(Chat::list(&paths, "ab12").is_empty());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn prompt_names_the_folders_tools_and_instructions() {
        let root = std::env::temp_dir().join(format!("superviser-prompt-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("AGENTS.md"), "Run `make check` before finishing.").unwrap();
        let project = Project::new(root.clone());
        let related = [Folder { path: "/a/docs".into(), writable: false }];
        let prompt = system_prompt(&project, &root, &related, false, true);
        assert!(prompt.contains("Also read-only: /a/docs"));
        assert!(prompt.contains("`edit`") && !prompt.contains("`bash`"));
        assert!(prompt.contains("Run `make check` before finishing."));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn multimodal_messages_derive_chat_titles() {
        let with_text = serde_json::json!([
            {"type": "text", "text": "Why is this button misaligned?"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
        ]);
        assert_eq!(
            user_message_title_source(&with_text).as_deref(),
            Some("Why is this button misaligned?")
        );
        let one_image = serde_json::json!([
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
        ]);
        assert_eq!(user_message_title_source(&one_image).as_deref(), Some("Image"));
        let two_images = serde_json::json!([
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
            {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,BBBB"}}
        ]);
        assert_eq!(user_message_title_source(&two_images).as_deref(), Some("2 images"));
    }
}
