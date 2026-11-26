//! Reusable agent skills stored in standard `SKILL.md` format (YAML frontmatter + Markdown body).
//!
//! Skills are discovered from both project folders (`.opencode/skills`, `.agents/skills`) and
//! global folders (`~/.config/opencode/skills`, plus the app's own `<data>/skills` fallback), so
//! skills created here work natively with OpenCode and with the built-in agent.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde_json::{Value, json};
use superviser_core::{opencode, secrets};

use crate::state::App;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillScope {
    Project,
    Global,
}

impl SkillScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::Project => "Project",
            Self::Global => "Global",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub instructions: String,
    pub scope: SkillScope,
    pub path: PathBuf,
}

/// Normalizes a user-typed skill name into a safe lowercase kebab-case slug.
pub fn slugify(raw: &str) -> String {
    let trimmed = raw.trim().trim_start_matches('/');
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in trimmed.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            out.push(lower);
            prev_dash = false;
        } else if (ch == '-' || ch == '_' || ch.is_whitespace()) && !out.is_empty() && !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Discovers all available project and global skills. Project skills take precedence over global
/// skills with the same slug.
pub fn discover(project_root: Option<&Path>, data_dir: &Path) -> Vec<Skill> {
    let mut dirs: Vec<(PathBuf, SkillScope)> = Vec::new();
    if let Some(root) = project_root {
        dirs.push((root.join(".opencode").join("skills"), SkillScope::Project));
        dirs.push((root.join(".agents").join("skills"), SkillScope::Project));
    }
    if let Some(oc) = opencode::config_dir() {
        dirs.push((oc.join("skills"), SkillScope::Global));
    }
    dirs.push((data_dir.join("skills"), SkillScope::Global));

    let mut seen = HashSet::new();
    let mut skills = Vec::new();

    for (base, scope) in dirs {
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        for entry in entries.flatten() {
            let dir_path = entry.path();
            if !dir_path.is_dir() {
                continue;
            }
            let skill_file = dir_path.join("SKILL.md");
            if !skill_file.is_file() {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&skill_file) else {
                continue;
            };
            let fallback_name = dir_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("skill");
            let Some(skill) = parse_skill_md(&raw, fallback_name, scope, skill_file) else {
                continue;
            };
            if seen.insert(skill.name.clone()) {
                skills.push(skill);
            }
        }
    }

    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
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
    let slug = slugify(name);
    if slug.is_empty() {
        bail!("Give the skill a name (letters, numbers, or dashes).");
    }
    if matches!(slug.as_str(), "new-skill" | "create-skill" | "skills" | "help") {
        bail!("`/{slug}` is reserved for a built-in command; pick a different name.");
    }
    let body = instructions.trim();
    if body.is_empty() {
        bail!("Add instructions describing what this skill should do.");
    }
    let desc = description.trim();
    let desc = if desc.is_empty() {
        body.lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("Custom skill")
            .trim()
            .trim_start_matches('#')
            .trim()
            .chars()
            .take(120)
            .collect::<String>()
    } else {
        desc.to_string()
    };

    let base_dir = match scope {
        SkillScope::Project => project_root
            .context("Select a project first to save a project skill.")?
            .join(".opencode")
            .join("skills"),
        SkillScope::Global => opencode::config_dir()
            .map(|d| d.join("skills"))
            .unwrap_or_else(|| data_dir.join("skills")),
    };

    let skill_dir = base_dir.join(&slug);
    std::fs::create_dir_all(&skill_dir)
        .with_context(|| format!("creating {}", skill_dir.display()))?;
    let path = skill_dir.join("SKILL.md");
    let content = render_skill_md(&slug, &desc, body);
    std::fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;

    Ok(Skill {
        name: slug,
        description: desc,
        instructions: body.to_string(),
        scope,
        path,
    })
}

/// Removes a skill's `SKILL.md` and cleans up its folder if now empty.
pub fn delete_skill(skill: &Skill) -> anyhow::Result<()> {
    if skill.path.exists() {
        std::fs::remove_file(&skill.path)
            .with_context(|| format!("removing {}", skill.path.display()))?;
    }
    if let Some(parent) = skill.path.parent() {
        let _ = std::fs::remove_dir(parent);
    }
    Ok(())
}

/// Formats an activated skill and optional user message into the prompt sent to the agent.
pub fn format_skill_prompt(skill: &Skill, user_args: &str) -> String {
    let args = user_args.trim();
    if args.is_empty() {
        format!(
            "Apply the `/{}` skill now.\n\n## Skill Instructions\n{}\n",
            skill.name, skill.instructions
        )
    } else {
        format!(
            "Use the `/{}` skill for this request.\n\n## Skill Instructions\n{}\n\n## User Request\n{}\n",
            skill.name, skill.instructions, args
        )
    }
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
    let (model, _) = preferred_model_id
        .and_then(|id| app.loaded(id))
        .or_else(|| app.latest_model())
        .ok_or_else(|| anyhow::anyhow!("Load a model first to draft a skill with AI."))?;
    let api_key = secrets::local_api_key(&app.paths)?;
    let url = format!("{}/chat/completions", guard_url.trim_end_matches('/'));

    let user_prompt = format!(
        "Skill name hint: {}\nWhat the skill should do: {}\n\nReturn ONLY a JSON object with keys `name` (short lowercase kebab-case slug), `description` (1 clear sentence under 100 chars), and `instructions` (concise, actionable Markdown steps for a coding agent).",
        if name_hint.trim().is_empty() { "(choose a short name)" } else { name_hint.trim() },
        goal.trim()
    );

    let body = json!({
        "model": model.alias,
        "messages": [
            {
                "role": "system",
                "content": "You create concise, practical reusable agent skills. Reply with valid JSON only (no markdown code fences): {\"name\":\"...\",\"description\":\"...\",\"instructions\":\"...\"}."
            },
            {
                "role": "user",
                "content": user_prompt
            }
        ],
        "stream": false,
        "temperature": 0.3,
        "max_tokens": 512,
        "reasoning_effort": "none",
        "chat_template_kwargs": { "enable_thinking": false }
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let response = client
        .post(&url)
        .bearer_auth(&api_key)
        .json(&body)
        .send()
        .await?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        bail!("Model returned {status}: {text}");
    }
    let payload: Value = response.json().await?;
    let raw = payload["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .trim();

    let json_text = extract_json_object(raw).unwrap_or(raw);
    let parsed: Value = serde_json::from_str(json_text)
        .context("The model didn't return valid JSON; try phrasing the goal more simply.")?;
    let drafted_name = slugify(
        parsed["name"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(name_hint),
    );
    let description = parsed["description"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();
    let instructions = parsed["instructions"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();
    if instructions.is_empty() {
        bail!("The model returned empty skill instructions.");
    }
    Ok((drafted_name, description, instructions))
}

fn extract_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (end > start).then(|| &text[start..=end])
}

fn render_skill_md(name: &str, description: &str, instructions: &str) -> String {
    let safe_desc = description.replace('\n', " ").replace('"', "\\\"");
    format!("---\nname: {name}\ndescription: \"{safe_desc}\"\n---\n\n{instructions}\n")
}

fn parse_skill_md(
    raw: &str,
    fallback_name: &str,
    scope: SkillScope,
    path: PathBuf,
) -> Option<Skill> {
    let mut name = slugify(fallback_name);
    let mut description = String::new();
    let body;

    let normalized = raw.replace("\r\n", "\n");
    if let Some(after_open) = normalized.strip_prefix("---\n")
        && let Some((fm, rest)) = after_open.split_once("\n---\n")
    {
        for line in fm.lines() {
            if let Some((k, v)) = line.split_once(':') {
                let key = k.trim();
                let val = unquote_yaml(v.trim());
                match key {
                    "name" if !val.is_empty() => {
                        let s = slugify(&val);
                        if !s.is_empty() {
                            name = s;
                        }
                    }
                    "description" => description = val,
                    _ => {}
                }
            }
        }
        body = rest.trim().to_string();
    } else {
        body = normalized.trim().to_string();
    }

    if name.is_empty() || body.is_empty() {
        return None;
    }
    if description.is_empty() {
        description = body
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim()
            .trim_start_matches('#')
            .trim()
            .chars()
            .take(120)
            .collect();
    }

    Some(Skill {
        name,
        description,
        instructions: body,
        scope,
        path,
    })
}

fn unquote_yaml(v: &str) -> String {
    if v.len() >= 2 && ((v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\''))) {
        v[1..v.len() - 1].replace("\\\"", "\"")
    } else {
        v.to_string()
    }
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
