//! A project: the folders an agent works on, how it is isolated, and what it may do there.
//!
//! The rules are enforced twice. OpenCode is told (its tools and MCP servers, through
//! `OPENCODE_CONFIG_CONTENT`, which beats a project's own `opencode.json`), and the guard
//! blocks whatever gets past that: OpenCode talks to it at `/projects/<id>/v1`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::opencode::{self, McpServer};
use crate::sandbox::{Isolation, Network};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    pub path: PathBuf,
    /// The agent may change it; otherwise it only reads it.
    pub writable: bool,
}

/// A kind of OpenCode tool that can be turned off per project.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ToolGroup {
    pub key: &'static str,
    pub label: &'static str,
    /// OpenCode's permission keys for it.
    #[serde(skip)]
    pub permissions: &'static [&'static str],
    /// Tool names the guard blocks when it's off: OpenCode's, and other harnesses' for the same thing.
    #[serde(skip)]
    pub names: &'static [&'static str],
}

pub const TOOL_GROUPS: &[ToolGroup] = &[
    ToolGroup { key: "commands", label: "Run commands in a shell", permissions: &["bash"], names: &["bash", "shell"] },
    ToolGroup { key: "edits", label: "Change files", permissions: &["edit"], names: &["edit", "write", "patch", "apply_patch", "multiedit"] },
    ToolGroup { key: "web", label: "Fetch and search web pages", permissions: &["webfetch", "websearch"], names: &["webfetch", "websearch"] },
    ToolGroup { key: "subagents", label: "Hand work to sub-agents", permissions: &["task"], names: &["task", "subagent"] },
];

/// When the built-in agent asks before acting. The guard and sandbox check every call either way.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Approvals {
    /// Before shell commands; file changes go ahead.
    #[default]
    Commands,
    /// Before commands and file changes.
    Everything,
    Never,
}

/// Programs worth suggesting for a block list: they reach other machines, accounts or
/// containers, or act as another user.
pub const SUGGESTED_PROGRAMS: &[&str] = &["ssh", "scp", "rsync", "sudo", "docker", "kubectl", "terraform", "aws", "gcloud", "gh"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    /// The first is the main folder, where OpenCode starts; it is always writable.
    pub folders: Vec<Folder>,
    #[serde(default)]
    pub isolation: Isolation,
    #[serde(default)]
    pub network: Network,
    /// Sites its calls may reach even when the conversation doesn't mention them.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// Keys of the [`TOOL_GROUPS`] turned off.
    #[serde(default)]
    pub tools_off: Vec<String>,
    /// Programs its commands may not run, by name.
    #[serde(default)]
    pub blocked_programs: Vec<String>,
    /// Global OpenCode MCP servers turned off for it.
    #[serde(default)]
    pub mcp_off: Vec<String>,
    /// When the built-in agent asks first.
    #[serde(default)]
    pub approvals: Approvals,
}

impl Project {
    /// A project for `folder`, named after it, with the default rules.
    pub fn new(folder: PathBuf) -> Self {
        let name = folder.file_name().map_or_else(|| folder.display().to_string(), |n| n.to_string_lossy().into_owned());
        let mut id = [0u8; 6];
        getrandom::fill(&mut id).expect("system randomness");
        Self {
            id: hex::encode(id),
            name,
            folders: vec![Folder { path: folder, writable: true }],
            isolation: Isolation::default(),
            network: Network::default(),
            allowed_hosts: Vec::new(),
            tools_off: Vec::new(),
            blocked_programs: Vec::new(),
            mcp_off: Vec::new(),
            approvals: Approvals::default(),
        }
    }

    /// Whether a group of [`TOOL_GROUPS`] is on.
    pub fn allows(&self, group: &str) -> bool {
        !self.tools_off.iter().any(|k| k == group)
    }

    pub fn main(&self) -> &Path {
        &self.folders[0].path
    }

    /// The folders besides the main one.
    pub fn related(&self) -> &[Folder] {
        &self.folders[1..]
    }

    /// Tidies what the user typed and checks the rest: a main folder, the main one writable,
    /// hosts as bare lowercase names, programs as bare names, ids that are ours.
    pub fn normalized(mut self) -> anyhow::Result<Self> {
        anyhow::ensure!(!self.folders.is_empty(), "a project needs a folder");
        anyhow::ensure!(!self.id.is_empty() && self.id.bytes().all(|b| b.is_ascii_alphanumeric()), "bad project id");
        self.name = self.name.trim().to_string();
        if self.name.is_empty() {
            self.name = Project::new(self.main().to_path_buf()).name;
        }
        self.folders[0].writable = true;
        let mut seen = std::collections::HashSet::new();
        self.folders.retain(|f| seen.insert(f.path.clone()));
        self.allowed_hosts = tidy(self.allowed_hosts.iter().map(|h| host(h)));
        self.blocked_programs = tidy(self.blocked_programs.iter().map(|p| p.trim().rsplit('/').next().unwrap_or("").to_string()));
        self.tools_off.retain(|k| TOOL_GROUPS.iter().any(|g| g.key == k));
        self.mcp_off = tidy(self.mcp_off.iter().cloned());
        Ok(self)
    }

    /// Tool names the guard blocks for it.
    pub fn blocked_tools(&self) -> Vec<String> {
        self.off_groups().flat_map(|g| g.names.iter().map(|n| n.to_string())).collect()
    }

    fn off_groups(&self) -> impl Iterator<Item = &'static ToolGroup> + '_ {
        TOOL_GROUPS.iter().filter(|g| self.tools_off.iter().any(|k| k == g.key))
    }

    /// Where OpenCode reaches the guard for it, after the guard's address.
    pub fn guard_path(&self) -> String {
        format!("/projects/{}", self.id)
    }

    /// OpenCode config for this project (for `OPENCODE_CONFIG_CONTENT`): when it asks first,
    /// its turned-off tools denied and MCP servers off. `servers` are the global ones.
    pub fn opencode_config(&self, servers: &[McpServer]) -> Value {
        let mut config = json!({});
        let (commands, edits) = match self.approvals {
            Approvals::Commands => ("ask", "allow"),
            Approvals::Everything => ("ask", "ask"),
            Approvals::Never => ("allow", "allow"),
        };
        let mut permission = serde_json::Map::from_iter([("bash".to_string(), json!(commands)), ("edit".to_string(), json!(edits))]);
        permission.extend(self.off_groups().flat_map(|g| g.permissions.iter().map(|p| (p.to_string(), json!("deny")))));
        config["permission"] = Value::Object(permission);
        let off: Vec<&McpServer> = servers.iter().filter(|s| self.mcp_off.contains(&s.name)).collect();
        if !off.is_empty() {
            config["mcp"] = opencode::mcp_off(off);
        }
        config
    }
}

/// `https://Docs.Example.com:443/path` → `docs.example.com`.
fn host(text: &str) -> String {
    let text = text.trim();
    let text = text.split_once("://").map_or(text, |(_, rest)| rest);
    let text = text.split(['/', '?', '#']).next().unwrap_or("");
    let text = text.rsplit('@').next().unwrap_or("");
    let text = text.split(':').next().unwrap_or("");
    text.trim_start_matches("*.").trim_end_matches('.').to_ascii_lowercase()
}

/// Without empty entries and duplicates, in order.
fn tidy(items: impl Iterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in items.map(|i| i.trim().to_string()) {
        if !item.is_empty() && !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_projects_are_named_after_their_folder() {
        let project = Project::new("/Users/a/code/site".into());
        assert_eq!(project.name, "site");
        assert_eq!(project.id.len(), 12);
        assert_eq!(project.guard_path(), format!("/projects/{}", project.id));
        assert!(project.folders[0].writable && project.related().is_empty());
    }

    #[test]
    fn normalizes_what_was_typed() {
        let mut project = Project::new("/a/site".into());
        project.name = "  ".into();
        project.folders[0].writable = false;
        project.folders.push(Folder { path: "/a/docs".into(), writable: false });
        project.folders.push(Folder { path: "/a/docs".into(), writable: true });
        project.allowed_hosts = vec!["https://Docs.Example.com:443/guide".into(), "*.npmjs.org".into(), "".into(), "docs.example.com".into()];
        project.blocked_programs = vec![" /usr/bin/ssh ".into(), "docker".into(), "ssh".into()];
        project.tools_off = vec!["web".into(), "nonsense".into()];
        let project = project.normalized().unwrap();
        assert_eq!(project.name, "site");
        assert!(project.folders[0].writable);
        assert_eq!(project.folders.len(), 2);
        assert_eq!(project.allowed_hosts, ["docs.example.com", "npmjs.org"]);
        assert_eq!(project.blocked_programs, ["ssh", "docker"]);
        assert_eq!(project.tools_off, ["web"]);
        assert!(Project { folders: vec![], ..project.clone() }.normalized().is_err());
        assert!(Project { id: "../x".into(), ..project }.normalized().is_err());
    }

    #[test]
    fn opencode_config_denies_tools_and_turns_off_servers() {
        let mut project = Project::new("/a/site".into());
        assert_eq!(project.opencode_config(&[]), json!({"permission": {"bash": "ask", "edit": "allow"}}));
        project.approvals = Approvals::Everything;
        assert_eq!(project.opencode_config(&[])["permission"]["edit"], "ask");
        project.tools_off = vec!["commands".into(), "web".into()];
        project.mcp_off = vec!["media".into()];
        let servers = opencode::parse_mcp(r#"{"mcp": {"media": {"type": "local", "command": ["/x"]}, "docs": {"type": "remote", "url": "https://d"}}}"#);
        let config = project.opencode_config(&servers);
        assert_eq!(config["permission"], json!({"bash": "deny", "edit": "ask", "webfetch": "deny", "websearch": "deny"}));
        assert_eq!(config["mcp"], json!({"media": {"type": "local", "command": ["/x"], "enabled": false}}));
        assert_eq!(project.blocked_tools(), ["bash", "shell", "webfetch", "websearch"]);
    }
}
