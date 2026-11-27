//! Lean system prompt builder tuned for local models (~250 tokens base).
//!
//! Supports project instruction files (`AGENTS.md`, `CLAUDE.md`) and Entropy/Pi-style project
//! prompt overrides (`.entropy/SYSTEM.md` or `.pi/SYSTEM.md` to replace, `APPEND_SYSTEM.md` to append).

use std::path::{Path, PathBuf};

use crate::tools::{WorkspaceFolder, clip};

/// Maximum characters of a project instruction file injected into the prompt.
const MAX_INSTRUCTIONS: usize = 8_000;
/// Files a project can leave instructions for agents in; first found wins.
const INSTRUCTION_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md"];
/// Optional full-prompt override locations (first found replaces default base prompt).
const SYSTEM_OVERRIDE_FILES: &[&str] = &[".entropy/SYSTEM.md", ".pi/SYSTEM.md"];
/// Optional prompt-append locations (first found appends to the prompt).
const SYSTEM_APPEND_FILES: &[&str] = &[".entropy/APPEND_SYSTEM.md", ".pi/APPEND_SYSTEM.md"];

/// Configuration for building the harness system prompt.
#[derive(Debug, Clone)]
pub struct PromptConfig {
    pub project_name: String,
    pub main_folder: PathBuf,
    /// Display path for the main folder (e.g. `/workspace` inside a Linux VM, or host path).
    pub display_main: String,
    pub related: Vec<WorkspaceFolder>,
    /// Optional display paths matching `related` (same order).
    pub display_related: Vec<(String, bool)>,
    pub commands: bool,
    pub edits: bool,
    pub subagents: bool,
    pub internet: bool,
    /// Short description of the execution environment, e.g. "macOS sandbox" or "Linux VM".
    pub environment: String,
    /// Shell label for bash tool description, e.g. "zsh on macOS" or "bash in Linux VM".
    pub shell_label: String,
}

impl PromptConfig {
    pub fn new(project_name: impl Into<String>, main_folder: PathBuf) -> Self {
        let display_main = main_folder.display().to_string();
        Self {
            project_name: project_name.into(),
            main_folder,
            display_main,
            related: Vec::new(),
            display_related: Vec::new(),
            commands: true,
            edits: true,
            subagents: true,
            internet: false,
            environment: "a sandbox".into(),
            shell_label: "zsh on macOS".into(),
        }
    }
}

/// Builds the lean system prompt for a project.
pub fn build(cfg: &PromptConfig) -> String {
    // Pi-style full system prompt override if the project explicitly defines one.
    for rel in SYSTEM_OVERRIDE_FILES {
        if let Ok(custom) = std::fs::read_to_string(cfg.main_folder.join(rel)) {
            let trimmed = custom.trim();
            if !trimmed.is_empty() {
                return clip(trimmed, MAX_INSTRUCTIONS * 2);
            }
        }
    }

    let mut prompt = format!(
        "You are Entropy, a coding agent working on \"{name}\". \
         Work in small, verifiable steps: inspect relevant code before editing, make focused changes, \
         and verify with builds or tests when possible. Keep replies concise and plain; state what you changed and what you couldn't verify.\n\n\
         Main folder: {main} (relative paths and commands start here).\n",
        name = cfg.project_name,
        main = cfg.display_main,
    );

    if !cfg.display_related.is_empty() {
        for (path, writable) in &cfg.display_related {
            prompt.push_str(&format!(
                "Also {}: {path}\n",
                if *writable { "yours to change" } else { "read-only" }
            ));
        }
    } else {
        for folder in &cfg.related {
            prompt.push_str(&format!(
                "Also {}: {}\n",
                if folder.writable { "yours to change" } else { "read-only" },
                folder.path.display()
            ));
        }
    }

    prompt.push_str("\nTools: `read`, `glob`, and `grep` to inspect code");
    if cfg.edits {
        prompt.push_str("; `edit` (targeted single or multi-hunk replacements; read first) and `write` for files");
    }
    if cfg.commands {
        prompt.push_str(&format!("; `bash` ({}) for commands", cfg.shell_label));
    }
    if cfg.subagents {
        prompt.push_str("; `subagent` for isolated read-only research when inspecting many files");
    }
    prompt.push_str(".\n");

    if !cfg.edits {
        prompt.push_str("This project is read-only: suggest changes instead of editing files.\n");
    }

    prompt.push_str(&format!(
        "\nEnvironment: commands run in {env} (writes limited to project writable folders and temp space; credentials and private files are blocked). \
         Internet is {net}. A guard checks every tool call before execution; if a call is blocked, explain what you wanted to do rather than working around it.\n",
        env = cfg.environment,
        net = if cfg.internet { "on" } else { "off (local only)" },
    ));

    append_project_instructions(&cfg.main_folder, &mut prompt);

    for rel in SYSTEM_APPEND_FILES {
        if let Ok(extra) = std::fs::read_to_string(cfg.main_folder.join(rel)) {
            let trimmed = extra.trim();
            if !trimmed.is_empty() {
                prompt.push_str(&format!("\n{}\n", clip(trimmed, MAX_INSTRUCTIONS)));
                break;
            }
        }
    }

    prompt
}

fn append_project_instructions(main: &Path, prompt: &mut String) {
    for name in INSTRUCTION_FILES {
        if let Ok(text) = std::fs::read_to_string(main.join(name)) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                prompt.push_str(&format!("\nProject instructions ({name}):\n\n{}\n", clip(trimmed, MAX_INSTRUCTIONS)));
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_stays_lean_and_loads_agents_md() {
        let root = std::env::temp_dir().join(format!("entropy-prompt-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("AGENTS.md"), "Run `cargo test` before finishing.").unwrap();
        let mut cfg = PromptConfig::new("Demo", root.clone());
        cfg.related = vec![WorkspaceFolder { path: "/a/docs".into(), writable: false }];
        cfg.commands = false;
        let p = build(&cfg);
        assert!(p.contains("Also read-only: /a/docs"));
        assert!(p.contains("`edit`") && !p.contains("`bash`"));
        assert!(p.contains("Run `cargo test` before finishing."));
        // Base prompt + short AGENTS.md should stay well under 2000 chars (~500 tokens).
        assert!(p.len() < 2000, "prompt too long: {} chars", p.len());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn system_md_override_replaces_default_prompt() {
        let root = std::env::temp_dir().join(format!("entropy-sysmd-{}", std::process::id()));
        std::fs::create_dir_all(root.join(".entropy")).unwrap();
        std::fs::write(root.join(".entropy/SYSTEM.md"), "Custom ultra-minimal prompt.").unwrap();
        let cfg = PromptConfig::new("Demo", root.clone());
        assert_eq!(build(&cfg), "Custom ultra-minimal prompt.");
        std::fs::remove_dir_all(root).ok();
    }
}
