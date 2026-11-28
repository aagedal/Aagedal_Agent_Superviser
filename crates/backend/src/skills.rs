//! Reusable agent skills stored in standard `SKILL.md` format (YAML frontmatter + Markdown body).
//!
//! Core discovery, parsing, formatting, and AI drafting live in [`entropy::skills`] so Agent
//! Superviser and the standalone Entropy TUI share identical skill behavior. This module adapts
//! them to Agent Superviser's app data directory, OpenCode compatibility path, and live `App` state.

use std::path::{Path, PathBuf};

use anyhow::Context;
use entropy::ModelEndpoint;
pub use entropy::skills::{Skill, SkillScope, delete_skill, format_skill_prompt, slugify};
use superviser_core::{opencode, secrets};

use crate::state::App;

/// Discovers all available project (`.entropy/skills`, `.agents/skills`, `.opencode/skills`)
/// and global skills. Project skills take precedence over global skills with the same slug.
pub fn discover(project_root: Option<&Path>, data_dir: &Path) -> Vec<Skill> {
    let mut global_dirs: Vec<PathBuf> = Vec::new();
    if let Some(oc) = opencode::config_dir() {
        global_dirs.push(oc.join("skills"));
    }
    global_dirs.push(data_dir.join("skills"));
    for dir in entropy::skills::default_global_dirs() {
        if !global_dirs.contains(&dir) {
            global_dirs.push(dir);
        }
    }
    entropy::skills::discover_in(project_root, &global_dirs)
}

/// Saves (creates or updates) a skill as `<dir>/<slug>/SKILL.md`.
pub fn save_skill(
    scope: SkillScope,
    project_root: Option<&Path>,
    data_dir: &Path,
    name: &str,
    description: &str,
    instructions: &str,
) -> anyhow::Result<Skill> {
    let base_dir = match scope {
        SkillScope::Project => project_root
            .context("Select a project first to save a project skill.")?
            .join(".opencode")
            .join("skills"),
        SkillScope::Global => opencode::config_dir()
            .map(|d| d.join("skills"))
            .unwrap_or_else(|| data_dir.join("skills")),
    };
    entropy::skills::save_skill_in(&base_dir, scope, name, description, instructions)
}

/// Uses the currently loaded model (in a separate, tool-free side request) to draft a reusable
/// skill from a short idea/goal. Returns `(slug_name, description, instructions)`.
pub async fn draft_skill_with_model(
    app: &App,
    name_hint: &str,
    goal: &str,
    preferred_model_id: Option<&str>,
) -> anyhow::Result<(String, String, String)> {
    let guard_url = app
        .guard_url()
        .ok_or_else(|| anyhow::anyhow!("The guard isn't running."))?;
    let (model, context_window) = preferred_model_id
        .and_then(|id| app.loaded(id))
        .or_else(|| app.latest_model())
        .ok_or_else(|| anyhow::anyhow!("Load a model first to draft a skill with AI."))?;
    let endpoint = ModelEndpoint {
        completions_url: format!("{}/chat/completions", guard_url.trim_end_matches('/')),
        api_key: secrets::local_api_key(&app.paths)?,
        model: model.alias,
        context_window,
    };
    entropy::skills::draft_skill(&endpoint, name_hint, goal).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugifies_names_cleanly() {
        assert_eq!(slugify("/Code Review!"), "code-review");
        assert_eq!(slugify("  my_skill  "), "my-skill");
        assert_eq!(slugify("rust--tests"), "rust-tests");
        assert_eq!(slugify("///"), "");
    }

    #[test]
    fn saves_discovers_overrides_and_deletes_skills() {
        let base = std::env::temp_dir().join(format!("superviser-skills-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let proj = base.join("proj");
        let data = base.join("data");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(&data).unwrap();

        let saved = save_skill(
            SkillScope::Project,
            Some(&proj),
            &data,
            "Code Review",
            "Review changes for bugs and clarity.",
            "1. Run git diff\n2. List risks before suggesting edits.",
        )
        .unwrap();
        assert_eq!(saved.name, "code-review");
        assert!(saved.path.ends_with(".opencode/skills/code-review/SKILL.md"));

        let found = discover(Some(&proj), &data);
        let mine = found.iter().find(|s| s.name == "code-review").unwrap();
        assert_eq!(mine.scope, SkillScope::Project);
        assert_eq!(mine.description, "Review changes for bugs and clarity.");
        assert!(mine.instructions.contains("Run git diff"));

        let formatted = format_skill_prompt(mine, "check chat.rs");
        assert!(formatted.contains("/code-review"));
        assert!(formatted.contains("check chat.rs"));

        delete_skill(mine).unwrap();
        assert!(!saved.path.exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
