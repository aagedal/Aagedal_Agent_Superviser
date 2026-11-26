//! Host-side Git and GitHub PR actions for the active project.
//!
//! These run outside the agent's Seatbelt/Linux VM sandbox on explicit user clicks so the
//! agent stays isolated from SSH keys, git credentials, keychains, and `gh` tokens while the
//! user can commit, push, and create or open pull requests directly from the chat window.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use superviser_core::sandbox::seatbelt;
use superviser_core::secrets;
use tokio::process::Command;

use crate::state::App;

const GIT_TIMEOUT: Duration = Duration::from_secs(60);
const GH_TIMEOUT: Duration = Duration::from_secs(30);
const PR_CHECK_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_DIFF_CHARS: usize = 6_000;

/// One changed path reported by `git status --porcelain=v2`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitFileChange {
    pub path: String,
    /// Short status code (`M`, `A`, `D`, `R`, `?`, `U`).
    pub status: String,
    pub staged: bool,
}

/// Summary of an open/existing GitHub pull request for the current branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestInfo {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub state: String,
}

/// Snapshot of a project's Git working tree and remote state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitStatus {
    pub is_repo: bool,
    pub branch: Option<String>,
    pub detached: bool,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub staged: usize,
    pub unstaged: usize,
    pub untracked: usize,
    pub dirty_files: Vec<GitFileChange>,
    pub has_remote: bool,
    pub gh_available: bool,
    pub open_pr: Option<PullRequestInfo>,
}

impl GitStatus {
    pub fn dirty_count(&self) -> usize {
        self.dirty_files.len()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitResult {
    pub hash: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushResult {
    pub branch: String,
    pub remote: String,
    pub set_upstream: bool,
    pub output: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrDraft {
    pub title: String,
    pub body: String,
}

/// Resolves `program` preferring Homebrew (`/opt/homebrew/bin`, `/usr/local/bin`) and the
/// current `PATH` before `/usr/bin` (whose `git` is Apple's `xcrun` shim), while still
/// falling back to the user's login-shell `PATH` for Finder-launched apps.
fn resolve_program(program: &str) -> Option<std::path::PathBuf> {
    let parent = std::env::var_os("PATH").unwrap_or_default();
    let login = seatbelt::login_path();
    let preferred = ["/opt/homebrew/bin", "/usr/local/bin"];
    let system = ["/usr/bin", "/bin"];
    // When launched from Finder, parent PATH is minimal (`/usr/bin:/bin:...`), so check
    // Homebrew locations before falling back to system shims.
    let parent_dirs: Vec<std::path::PathBuf> = std::env::split_paths(&parent).collect();
    let non_system_parent = parent_dirs
        .iter()
        .filter(|d| !matches!(d.to_str(), Some("/usr/bin" | "/bin" | "/usr/sbin" | "/sbin")))
        .cloned();
    non_system_parent
        .chain(preferred.into_iter().map(std::path::PathBuf::from))
        .chain(std::env::split_paths(&login))
        .chain(system.into_iter().map(std::path::PathBuf::from))
        .map(|dir| dir.join(program))
        .find(|p| p.is_file())
}

fn host_path() -> String {
    let login = seatbelt::login_path();
    let mut parts: Vec<String> = Vec::new();
    for dir in ["/opt/homebrew/bin", "/usr/local/bin"] {
        if Path::new(dir).is_dir() {
            parts.push(dir.to_string());
        }
    }
    if let Ok(parent) = std::env::var("PATH") {
        for seg in parent.split(':').filter(|s| !s.is_empty()) {
            if !parts.iter().any(|p| p == seg) {
                parts.push(seg.to_string());
            }
        }
    }
    for seg in login.split(':').filter(|s| !s.is_empty()) {
        if !parts.iter().any(|p| p == seg) {
            parts.push(seg.to_string());
        }
    }
    parts.join(":")
}

/// Builds a host command with the resolved executable path, an enriched `PATH`,
/// and non-interactive git prompts disabled.
fn host_command(program: &str, cwd: &Path) -> Command {
    let bin = resolve_program(program).unwrap_or_else(|| std::path::PathBuf::from(program));
    let mut cmd = Command::new(bin);
    cmd.current_dir(cwd)
        .env("PATH", host_path())
        .env("GIT_TERMINAL_PROMPT", "0");
    cmd
}

async fn run_output(
    program: &str,
    args: &[&str],
    cwd: &Path,
    timeout: Duration,
) -> anyhow::Result<std::process::Output> {
    let child = host_command(program, cwd)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting `{program}`"))?;
    tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .with_context(|| format!("`{program} {}` timed out", args.join(" ")))?
        .with_context(|| format!("waiting for `{program}`"))
}

fn find_program(name: &str) -> bool {
    resolve_program(name).is_some()
}

/// Fast local-only Git status for `root` (no network calls).
pub async fn status(root: &Path) -> GitStatus {
    if !root.is_dir() {
        return GitStatus::default();
    }
    let Ok(inside) = run_output(
        "git",
        &["rev-parse", "--is-inside-work-tree"],
        root,
        Duration::from_secs(5),
    )
    .await
    else {
        return GitStatus::default();
    };
    if !inside.status.success() || String::from_utf8_lossy(&inside.stdout).trim() != "true" {
        return GitStatus::default();
    }

    let mut st = match run_output(
        "git",
        &["status", "--porcelain=v2", "--branch"],
        root,
        Duration::from_secs(10),
    )
    .await
    {
        Ok(out) if out.status.success() => parse_porcelain_v2(&String::from_utf8_lossy(&out.stdout)),
        _ => GitStatus {
            is_repo: true,
            ..Default::default()
        },
    };
    st.is_repo = true;
    st.has_remote = run_output("git", &["remote"], root, Duration::from_secs(5))
        .await
        .is_ok_and(|o| o.status.success() && !String::from_utf8_lossy(&o.stdout).trim().is_empty());
    st.gh_available = find_program("gh");
    st
}

/// Checks GitHub for an existing pull request on the current branch via `gh pr view`.
/// Returns `None` cleanly when offline, unauthenticated, or no PR exists.
pub async fn current_pr(root: &Path) -> Option<PullRequestInfo> {
    if !find_program("gh") {
        return None;
    }
    let out = run_output(
        "gh",
        &["pr", "view", "--json", "number,title,url,state"],
        root,
        PR_CHECK_TIMEOUT,
    )
    .await
    .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    let number = v["number"].as_u64()?;
    let url = v["url"].as_str()?.to_string();
    let title = v["title"].as_str().unwrap_or("").to_string();
    let state = v["state"].as_str().unwrap_or("OPEN").to_string();
    Some(PullRequestInfo {
        number,
        title,
        url,
        state,
    })
}

/// Stages changes (when `stage_all` is true) and commits with `message`.
pub async fn commit(root: &Path, message: &str, stage_all: bool) -> anyhow::Result<CommitResult> {
    let message = message.trim();
    if message.is_empty() {
        bail!("Enter a commit message first.");
    }
    if stage_all {
        let add = run_output("git", &["add", "-A"], root, GIT_TIMEOUT).await?;
        if !add.status.success() {
            let err = String::from_utf8_lossy(&add.stderr).trim().to_string();
            bail!("git add failed: {err}");
        }
    }
    let out = run_output("git", &["commit", "-m", message], root, GIT_TIMEOUT).await?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let detail = if !stderr.is_empty() { stderr } else { stdout };
        bail!("git commit failed: {detail}");
    }
    let hash = run_output(
        "git",
        &["rev-parse", "--short", "HEAD"],
        root,
        Duration::from_secs(5),
    )
    .await
    .ok()
    .filter(|o| o.status.success())
    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    .unwrap_or_default();
    let summary = message.lines().next().unwrap_or(message).to_string();
    Ok(CommitResult { hash, summary })
}

/// Pushes the current branch, automatically setting upstream to `origin` (or the first
/// configured remote) if no upstream tracking branch is set yet.
pub async fn push(root: &Path) -> anyhow::Result<PushResult> {
    let st = status(root).await;
    if !st.is_repo {
        bail!("This folder is not a Git repository.");
    }
    if st.detached {
        bail!("Cannot push from a detached HEAD. Switch to a branch first.");
    }
    let branch = st
        .branch
        .clone()
        .context("Could not determine the current Git branch.")?;

    if let Some(upstream) = &st.upstream {
        let out = run_output("git", &["push"], root, GIT_TIMEOUT).await?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            bail!("git push failed: {stderr}");
        }
        let remote = upstream.split('/').next().unwrap_or("origin").to_string();
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
        .trim()
        .to_string();
        return Ok(PushResult {
            branch,
            remote,
            set_upstream: false,
            output: combined,
        });
    }

    let remote_out = run_output("git", &["remote"], root, Duration::from_secs(5)).await?;
    let remotes: Vec<String> = String::from_utf8_lossy(&remote_out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    if remotes.is_empty() {
        bail!("No Git remote is configured for this repository.");
    }
    let remote = if remotes.iter().any(|r| r == "origin") {
        "origin".to_string()
    } else {
        remotes[0].clone()
    };
    let out = run_output("git", &["push", "-u", &remote, "HEAD"], root, GIT_TIMEOUT).await?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        bail!("git push failed: {stderr}");
    }
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
    .trim()
    .to_string();
    Ok(PushResult {
        branch,
        remote,
        set_upstream: true,
        output: combined,
    })
}

/// Pushes the branch if needed and creates a GitHub pull request via `gh pr create`.
pub async fn create_pr(
    root: &Path,
    title: &str,
    body: &str,
    draft: bool,
) -> anyhow::Result<PullRequestInfo> {
    let title = title.trim();
    if title.is_empty() {
        bail!("Enter a pull request title first.");
    }
    if !find_program("gh") {
        bail!("GitHub CLI (`gh`) was not found on your PATH. Install it with `brew install gh` and run `gh auth login`.");
    }
    let st = status(root).await;
    if !st.is_repo {
        bail!("This folder is not a Git repository.");
    }
    if st.detached {
        bail!("Cannot open a pull request from a detached HEAD.");
    }
    if st.upstream.is_none() || st.ahead > 0 {
        push(root).await?;
    }

    let mut args = vec!["pr", "create", "--title", title, "--body", body.trim()];
    if draft {
        args.push("--draft");
    }
    let out = run_output("gh", &args, root, GH_TIMEOUT).await?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        bail!("gh pr create failed: {stderr}");
    }
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let url = stdout
        .lines()
        .rev()
        .find(|line| line.trim().starts_with("https://"))
        .map(|l| l.trim().to_string())
        .unwrap_or_default();
    if let Some(pr) = current_pr(root).await {
        return Ok(pr);
    }
    let number = parse_pr_number(&url).unwrap_or(0);
    Ok(PullRequestInfo {
        number,
        title: title.to_string(),
        url,
        state: if draft { "DRAFT" } else { "OPEN" }.into(),
    })
}

/// Uses the currently loaded local model (tool-free, non-thinking) to draft a commit message
/// from the working tree diff.
pub async fn generate_commit_message(
    app: &App,
    root: &Path,
    preferred_model_id: Option<&str>,
) -> anyhow::Result<String> {
    let summary = working_diff_summary(root, MAX_DIFF_CHARS).await?;
    let system = "Write a single concise Git commit message subject line (max 72 characters, imperative mood, no trailing period, no quotes, no markdown fences) describing these changes. Return only the commit message line.";
    let raw = complete_short(app, preferred_model_id, system, &summary, 96).await?;
    let cleaned = clean_single_line(&raw);
    if cleaned.is_empty() {
        bail!("The model returned an empty commit message.");
    }
    Ok(cleaned)
}

/// Drafts a PR title and short summary body from branch commits (using the loaded model if
/// available, otherwise falling back deterministically to the branch's commit log).
pub async fn generate_pr_draft(
    app: &App,
    root: &Path,
    preferred_model_id: Option<&str>,
) -> anyhow::Result<PrDraft> {
    let (commits, stat) = branch_summary(root).await;
    let fallback_title = commits
        .first()
        .cloned()
        .unwrap_or_else(|| "Update project".to_string());
    let fallback_body = if commits.len() > 1 {
        commits
            .iter()
            .map(|c| format!("- {c}"))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        String::new()
    };

    let has_model = preferred_model_id
        .and_then(|id| app.loaded(id))
        .or_else(|| app.latest_model())
        .is_some();
    if !has_model {
        return Ok(PrDraft {
            title: fallback_title,
            body: fallback_body,
        });
    }

    let prompt = format!(
        "Commits on this branch:\n{}\n\nDiff stat:\n{}",
        if commits.is_empty() {
            "(no commits listed)".into()
        } else {
            commits.join("\n")
        },
        truncate_chars(&stat, 3_000)
    );
    let system = "Draft a GitHub pull request title and concise bullet-point summary from these commits and diff stat. Respond in exactly this format:\nTITLE: <concise title under 72 chars>\nBODY:\n- <bullet 1>\n- <bullet 2>";
    let Ok(raw) = complete_short(app, preferred_model_id, system, &prompt, 260).await else {
        return Ok(PrDraft {
            title: fallback_title,
            body: fallback_body,
        });
    };
    Ok(parse_pr_draft(&raw, &fallback_title, &fallback_body))
}

async fn working_diff_summary(root: &Path, max_chars: usize) -> anyhow::Result<String> {
    let short = run_output("git", &["status", "--short"], root, Duration::from_secs(5)).await?;
    let short_txt = String::from_utf8_lossy(&short.stdout).trim().to_string();
    if short_txt.is_empty() {
        bail!("There are no uncommitted changes to describe.");
    }
    let diff = run_output("git", &["diff", "HEAD"], root, Duration::from_secs(10))
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            // Unborn repo without HEAD: fall back to unstaged/cached diff if any.
            String::new()
        });
    let combined = if diff.is_empty() {
        format!("Changed files:\n{short_txt}")
    } else {
        format!("Changed files:\n{short_txt}\n\nDiff:\n{diff}")
    };
    Ok(truncate_chars(&combined, max_chars))
}

async fn branch_summary(root: &Path) -> (Vec<String>, String) {
    // Compare against origin/HEAD (or main/master) if available; otherwise last 10 commits.
    let base = default_base_ref(root).await;
    let range = base
        .as_deref()
        .map(|b| format!("{b}..HEAD"))
        .unwrap_or_else(|| "HEAD~10..HEAD".into());
    let log_out = run_output(
        "git",
        &["log", "--format=%s", "-n", "15", &range],
        root,
        Duration::from_secs(5),
    )
    .await
    .ok()
    .filter(|o| o.status.success())
    .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    .unwrap_or_default();
    let commits: Vec<String> = if log_out.trim().is_empty() {
        run_output(
            "git",
            &["log", "--format=%s", "-n", "5"],
            root,
            Duration::from_secs(5),
        )
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
    } else {
        log_out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect()
    };
    let stat = if let Some(b) = base {
        run_output(
            "git",
            &["diff", "--stat", &format!("{b}...HEAD")],
            root,
            Duration::from_secs(5),
        )
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
    } else {
        String::new()
    };
    (commits, stat)
}

async fn default_base_ref(root: &Path) -> Option<String> {
    if let Ok(out) = run_output(
        "git",
        &["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"],
        root,
        Duration::from_secs(3),
    )
    .await
        && out.status.success()
    {
        let r = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !r.is_empty() {
            return Some(r);
        }
    }
    for candidate in ["origin/main", "origin/master", "main", "master"] {
        if run_output(
            "git",
            &["rev-parse", "--verify", "--quiet", candidate],
            root,
            Duration::from_secs(3),
        )
        .await
        .is_ok_and(|o| o.status.success())
        {
            return Some(candidate.into());
        }
    }
    None
}

async fn complete_short(
    app: &App,
    preferred_model_id: Option<&str>,
    system: &str,
    user: &str,
    max_tokens: u32,
) -> anyhow::Result<String> {
    let guard_url = app
        .guard_url()
        .ok_or_else(|| anyhow::anyhow!("The guard isn't running."))?;
    let (model, _) = preferred_model_id
        .and_then(|id| app.loaded(id))
        .or_else(|| app.latest_model())
        .ok_or_else(|| anyhow::anyhow!("Load a model first to draft messages automatically."))?;
    let api_key = secrets::local_api_key(&app.paths)?;
    let url = format!("{}/chat/completions", guard_url.trim_end_matches('/'));

    let body = json!({
        "model": model.alias,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user }
        ],
        "stream": false,
        "temperature": 0.2,
        "max_tokens": max_tokens,
        "reasoning_effort": "none",
        "chat_template_kwargs": { "enable_thinking": false }
    });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
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
    Ok(payload["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string())
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push_str("\n… (truncated)");
    out
}

fn clean_single_line(raw: &str) -> String {
    let line = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("```"))
        .unwrap_or("");
    line.trim_matches(['"', '\'', '`'])
        .trim_end_matches('.')
        .trim()
        .to_string()
}

fn parse_pr_draft(raw: &str, fallback_title: &str, fallback_body: &str) -> PrDraft {
    let mut title = None;
    let mut body_lines = Vec::new();
    let mut in_body = false;
    for line in raw.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed
            .strip_prefix("TITLE:")
            .or_else(|| trimmed.strip_prefix("Title:"))
        {
            let cleaned = clean_single_line(rest);
            if !cleaned.is_empty() {
                title = Some(cleaned);
            }
            in_body = false;
        } else if let Some(rest) = trimmed
            .strip_prefix("BODY:")
            .or_else(|| trimmed.strip_prefix("Body:"))
        {
            in_body = true;
            if !rest.trim().is_empty() {
                body_lines.push(rest.trim().to_string());
            }
        } else if in_body {
            body_lines.push(line.to_string());
        }
    }
    let body = body_lines.join("\n").trim().to_string();
    PrDraft {
        title: title.unwrap_or_else(|| fallback_title.to_string()),
        body: if body.is_empty() {
            fallback_body.to_string()
        } else {
            body
        },
    }
}

pub fn parse_pr_number(url: &str) -> Option<u64> {
    url.trim_end_matches('/')
        .rsplit('/')
        .next()?
        .parse::<u64>()
        .ok()
}

/// Parses `git status --porcelain=v2 --branch` output into a [`GitStatus`].
pub fn parse_porcelain_v2(output: &str) -> GitStatus {
    let mut st = GitStatus {
        is_repo: true,
        ..Default::default()
    };
    for line in output.lines() {
        if let Some(head) = line.strip_prefix("# branch.head ") {
            let head = head.trim();
            if head == "(detached)" {
                st.detached = true;
                st.branch = None;
            } else if !head.is_empty() {
                st.branch = Some(head.to_string());
            }
        } else if let Some(upstream) = line.strip_prefix("# branch.upstream ") {
            let u = upstream.trim();
            if !u.is_empty() {
                st.upstream = Some(u.to_string());
            }
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            for part in ab.split_whitespace() {
                if let Some(n) = part.strip_prefix('+') {
                    st.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix('-') {
                    st.behind = n.parse().unwrap_or(0);
                }
            }
        } else if let Some(rest) = line.strip_prefix("1 ") {
            // `1 XY sub mH mI mW hH hI <path>` (9 fields; path may contain spaces)
            let mut parts = rest.splitn(8, ' ');
            let xy = parts.next().unwrap_or("..");
            let path = parts.nth(6).unwrap_or("").to_string();
            record_xy(&mut st, xy, path);
        } else if let Some(rest) = line.strip_prefix("2 ") {
            // `2 XY sub mH mI mW hH hI Xscore <path>\t<origPath>`
            let mut parts = rest.splitn(9, ' ');
            let xy = parts.next().unwrap_or("..");
            let path_part = parts.nth(7).unwrap_or("");
            let path = path_part.split('\t').next().unwrap_or(path_part).to_string();
            record_xy(&mut st, xy, path);
        } else if let Some(rest) = line.strip_prefix("u ") {
            // Unmerged: `u XY sub m1 m2 m3 mW h1 h2 h3 <path>`
            let mut parts = rest.splitn(10, ' ');
            let _xy = parts.next();
            let path = parts.nth(8).unwrap_or("").to_string();
            if !path.is_empty() {
                st.unstaged += 1;
                st.dirty_files.push(GitFileChange {
                    path,
                    status: "U".into(),
                    staged: false,
                });
            }
        } else if let Some(path) = line.strip_prefix("? ") {
            let path = path.trim().to_string();
            if !path.is_empty() {
                st.untracked += 1;
                st.dirty_files.push(GitFileChange {
                    path,
                    status: "?".into(),
                    staged: false,
                });
            }
        }
    }
    st
}

fn record_xy(st: &mut GitStatus, xy: &str, path: String) {
    if path.is_empty() {
        return;
    }
    let bytes = xy.as_bytes();
    let x = bytes.first().copied().unwrap_or(b'.') as char;
    let y = bytes.get(1).copied().unwrap_or(b'.') as char;
    let is_staged = x != '.';
    let is_unstaged = y != '.';
    if is_staged {
        st.staged += 1;
    }
    if is_unstaged {
        st.unstaged += 1;
    }
    let code = if is_staged { x } else { y };
    st.dirty_files.push(GitFileChange {
        path,
        status: code.to_string(),
        staged: is_staged && !is_unstaged,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_v2_branch_and_changes() {
        let sample = "\
# branch.oid abcdef1234
# branch.head feat/git-ui
# branch.upstream origin/feat/git-ui
# branch.ab +2 -1
1 M. N... 100644 100644 100644 aaa bbb src/lib.rs
1 .M N... 100644 100644 100644 aaa bbb src/path with spaces.rs
2 R. N... 100644 100644 100644 aaa bbb R100 src/new.rs\tsrc/old.rs
? notes.md
";
        let st = parse_porcelain_v2(sample);
        assert!(st.is_repo);
        assert_eq!(st.branch.as_deref(), Some("feat/git-ui"));
        assert!(!st.detached);
        assert_eq!(st.upstream.as_deref(), Some("origin/feat/git-ui"));
        assert_eq!((st.ahead, st.behind), (2, 1));
        assert_eq!((st.staged, st.unstaged, st.untracked), (2, 1, 1));
        assert_eq!(st.dirty_files.len(), 4);
        assert_eq!(st.dirty_files[1].path, "src/path with spaces.rs");
        assert_eq!(st.dirty_files[2].path, "src/new.rs");
        assert_eq!(st.dirty_files[2].status, "R");
        assert_eq!(st.dirty_files[3].status, "?");
    }

    #[test]
    fn parses_detached_head_and_pr_draft() {
        let st = parse_porcelain_v2("# branch.head (detached)\n");
        assert!(st.detached);
        assert_eq!(st.branch, None);

        let draft = parse_pr_draft(
            "TITLE: Add git commit and PR bar.\nBODY:\n- Added host git backend\n- Wired GPUI buttons\n",
            "fallback",
            "fallback body",
        );
        assert_eq!(draft.title, "Add git commit and PR bar");
        assert_eq!(draft.body, "- Added host git backend\n- Wired GPUI buttons");
        assert_eq!(parse_pr_number("https://github.com/org/repo/pull/42"), Some(42));
    }

    #[tokio::test]
    async fn status_and_commit_work_on_temp_repo() {
        let dir = std::env::temp_dir().join(format!("superviser-git-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        assert!(!status(&dir).await.is_repo);

        let init = run_output("git", &["init", "-b", "main"], &dir, Duration::from_secs(5))
            .await
            .unwrap();
        assert!(init.status.success());
        run_output("git", &["config", "user.name", "Test"], &dir, Duration::from_secs(5))
            .await
            .unwrap();
        run_output("git", &["config", "user.email", "test@example.com"], &dir, Duration::from_secs(5))
            .await
            .unwrap();

        std::fs::write(dir.join("hello.txt"), "hello\n").unwrap();
        let before = status(&dir).await;
        assert!(before.is_repo);
        assert_eq!(before.branch.as_deref(), Some("main"));
        assert_eq!(before.untracked, 1);

        let res = commit(&dir, "Initial commit", true).await.unwrap();
        assert!(!res.hash.is_empty());
        assert_eq!(res.summary, "Initial commit");

        let after = status(&dir).await;
        assert_eq!(after.dirty_count(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
