//! Coding agent tools: `read`, `glob`, `grep`, `write`, `edit`, and sandboxed `bash`.
//!
//! Paths outside the project's folders are refused (with iterative symlink resolution so dangling
//! links cannot escape), and writes go only to writable folders. Shell execution is abstracted
//! behind [`CommandRunner`] so macOS Seatbelt, Linux VM (`container`), or test runners plug in
//! cleanly with zero platform coupling inside the harness.

use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::sync::watch;

use crate::edit::{self, EditSpec};

/// Characters of a tool result the model reads; longer ones keep their start and end.
const MAX_RESULT: usize = 30_000;
const MAX_READ_LINES: usize = 2000;
const MAX_LINE: usize = 2000;
const MAX_MATCHES: usize = 200;
const MAX_LISTED: usize = 500;
const MAX_READ_FILE: u64 = 16 << 20;
const MAX_SEARCHED_FILE: u64 = 2 << 20;
/// Bytes of command output kept from its start and its end.
pub const OUTPUT_HEAD: usize = 64 << 10;
pub const OUTPUT_TAIL: usize = 64 << 10;
pub const DEFAULT_TIMEOUT: u64 = 120;
pub const MAX_TIMEOUT: u64 = 600;

/// One folder visible to the workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceFolder {
    pub path: PathBuf,
    pub writable: bool,
}

/// What a tool does, for deciding whether to ask first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Reads,
    Changes,
    Runs,
}

pub fn kind(name: &str) -> Kind {
    match name {
        "write" | "edit" => Kind::Changes,
        "bash" => Kind::Runs,
        _ => Kind::Reads,
    }
}

/// Output of a shell command run through a [`CommandRunner`].
#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub output: String,
    pub exit_summary: String,
}

impl CommandOutput {
    pub fn format(self) -> String {
        if self.output.trim().is_empty() {
            format!("(no output, {})", self.exit_summary)
        } else {
            format!("{}\n({})", self.output.trim_end(), self.exit_summary)
        }
    }
}

/// Pluggable shell command runner (Seatbelt sandbox, Linux VM, or local test shell).
pub trait CommandRunner: Send + Sync {
    /// Human-readable description appended to the `bash` tool schema, e.g. `"zsh"` or `"bash in Linux VM"`.
    fn shell_description(&self) -> &str {
        "shell"
    }

    fn run(
        &self,
        command: &str,
        timeout: Duration,
        cancel: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, String>> + Send + '_>>;
}

/// Optional mapper translating virtual paths (such as `/workspace/...` in a Linux VM) to host paths.
pub trait PathMapper: Send + Sync {
    fn to_host(&self, path: &Path) -> Option<PathBuf>;
}

/// Helper for running a `std::process::Command` with process-group isolation, head+tail capture,
/// timeout, and cancellation. Reused by both Seatbelt and Linux VM runners.
pub async fn run_process(
    mut std_command: std::process::Command,
    timeout: Duration,
    mut cancel: watch::Receiver<bool>,
) -> Result<CommandOutput, String> {
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut std_command, 0);

    let mut child = tokio::process::Command::from(std_command)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("couldn't start the shell: {e}"))?;

    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let pid = child.id();

    struct GroupGuard(Option<u32>);
    impl Drop for GroupGuard {
        fn drop(&mut self) {
            #[cfg(unix)]
            if let Some(pid) = self.0 {
                let _ = std::process::Command::new("/bin/kill")
                    .args(["-KILL", &format!("-{pid}")])
                    .status();
            }
        }
    }
    let mut group_guard = GroupGuard(pid);

    let collect = async {
        let mut head = Vec::new();
        let mut tail = VecDeque::new();
        let mut total = 0usize;
        let mut push = |bytes: &[u8]| {
            total += bytes.len();
            for &byte in bytes {
                if head.len() < OUTPUT_HEAD {
                    head.push(byte);
                } else {
                    tail.push_back(byte);
                    if tail.len() > OUTPUT_TAIL {
                        tail.pop_front();
                    }
                }
            }
        };
        let mut out_buf = [0u8; 8192];
        let mut err_buf = [0u8; 8192];
        let (mut out_done, mut err_done) = (false, false);
        while !(out_done && err_done) {
            tokio::select! {
                res = stdout.read(&mut out_buf), if !out_done => match res {
                    Ok(0) | Err(_) => out_done = true,
                    Ok(n) => push(&out_buf[..n]),
                },
                res = stderr.read(&mut err_buf), if !err_done => match res {
                    Ok(0) | Err(_) => err_done = true,
                    Ok(n) => push(&err_buf[..n]),
                },
            }
        }
        let status = child.wait().await;
        (head, tail, total, status)
    };

    let (head, tail, total, status) = tokio::select! {
        done = collect => {
            group_guard.0 = None;
            done
        }
        _ = tokio::time::sleep(timeout) => return Err(format!("the command was stopped after {} s", timeout.as_secs())),
        _ = wait_cancelled(&mut cancel) => return Err("the user stopped the command".to_string()),
    };

    let mut output = String::from_utf8_lossy(&head).into_owned();
    if !tail.is_empty() {
        let cut = total - head.len() - tail.len();
        if cut > 0 {
            output.push_str(&format!("\n… [{cut} bytes cut] …\n"));
        }
        output.push_str(&String::from_utf8_lossy(&tail.into_iter().collect::<Vec<u8>>()));
    }
    let exit_summary = match status {
        Ok(status) => status
            .code()
            .map_or("killed by a signal".into(), |c| format!("exit code {c}")),
        Err(e) => format!("unknown exit ({e})"),
    };
    Ok(CommandOutput { output, exit_summary })
}

async fn wait_cancelled(cancel: &mut watch::Receiver<bool>) {
    if *cancel.borrow() {
        return;
    }
    while cancel.changed().await.is_ok() {
        if *cancel.borrow() {
            return;
        }
    }
    std::future::pending::<()>().await;
}

/// The folders the agent works in, and how it may use them.
pub struct Workspace {
    /// With symlinks resolved; the first is the main folder.
    folders: Vec<WorkspaceFolder>,
    /// Pluggable shell runner; `None` when commands are off.
    runner: Option<Arc<dyn CommandRunner>>,
    /// Optional virtual-path mapper (e.g. Linux VM `/workspace` → host path).
    path_mapper: Option<Arc<dyn PathMapper>>,
    edits: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Access {
    Read,
    Write,
}

impl Workspace {
    /// `folders` must exist; the first is the main one.
    pub fn new(
        folders: &[WorkspaceFolder],
        runner: Option<Arc<dyn CommandRunner>>,
        edits: bool,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(!folders.is_empty(), "a project needs a folder");
        let folders = folders
            .iter()
            .map(|f| {
                Ok(WorkspaceFolder {
                    path: std::fs::canonicalize(&f.path)
                        .map_err(|e| anyhow::anyhow!("{}: {e}", f.path.display()))?,
                    writable: f.writable,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Self { folders, runner, path_mapper: None, edits })
    }

    pub fn with_path_mapper(mut self, mapper: Arc<dyn PathMapper>) -> Self {
        self.path_mapper = Some(mapper);
        self
    }

    pub fn main(&self) -> &Path {
        &self.folders[0].path
    }

    /// The tools to offer the model, in OpenAI function-calling format.
    pub fn definitions(&self) -> Vec<Value> {
        let tool = |name: &str, description: &str, properties: Value, required: &[&str]| {
            json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": {
                        "type": "object",
                        "properties": properties,
                        "required": required
                    }
                }
            })
        };
        let mut tools = vec![
            tool(
                "read",
                "Read a text file (numbered lines) or list a folder. Paths are relative to the main folder unless absolute.",
                json!({
                    "path": { "type": "string" },
                    "offset": { "type": "integer", "description": "First line to read, from 1" },
                    "limit": { "type": "integer", "description": "Lines to read (default 2000)" }
                }),
                &["path"],
            ),
            tool(
                "glob",
                "Find files by name pattern, like `**/*.rs` or `src/**/test_*.py`. Skips what .gitignore ignores.",
                json!({
                    "pattern": { "type": "string" },
                    "path": { "type": "string", "description": "Folder to search (default: the main folder)" }
                }),
                &["pattern"],
            ),
            tool(
                "grep",
                "Search file contents with a regular expression. Returns `file:line: text`. Skips what .gitignore ignores.",
                json!({
                    "pattern": { "type": "string", "description": "Regular expression; start with (?i) to ignore case" },
                    "path": { "type": "string", "description": "File or folder to search (default: the main folder)" },
                    "include": { "type": "string", "description": "Only files matching this glob, like `*.ts`" }
                }),
                &["pattern"],
            ),
        ];
        if self.edits {
            tools.push(tool(
                "write",
                "Create a file or replace all of it. For changes to an existing file, prefer `edit`.",
                json!({
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                }),
                &["path", "content"],
            ));
            tools.push(tool(
                "edit",
                "Replace text in a file (single edit via `old_string`/`new_string`, or atomic multi-hunk via `edits`). Include enough context to match uniquely. Read the file first.",
                json!({
                    "path": { "type": "string" },
                    "old_string": { "type": "string" },
                    "new_string": { "type": "string" },
                    "replace_all": { "type": "boolean" },
                    "edits": {
                        "type": "array",
                        "description": "Optional list of {old_string, new_string, replace_all?} applied atomically in order",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_string": { "type": "string" },
                                "new_string": { "type": "string" },
                                "replace_all": { "type": "boolean" }
                            },
                            "required": ["old_string", "new_string"]
                        }
                    }
                }),
                &["path"],
            ));
        }
        if let Some(runner) = &self.runner {
            tools.push(tool(
                "bash",
                &format!(
                    "Run a shell command ({}) from the main folder, in the project's sandbox. Use it to build, test, and run git. Output is cut when long.",
                    runner.shell_description()
                ),
                json!({
                    "command": { "type": "string" },
                    "timeout": { "type": "integer", "description": "Seconds before it is stopped (default 120, at most 600)" }
                }),
                &["command"],
            ));
        }
        tools
    }

    /// Runs a tool call; the returned text is what the model reads, errors included.
    pub async fn run(&self, name: &str, arguments: &str, cancel: watch::Receiver<bool>) -> String {
        let args: Value = match serde_json::from_str(if arguments.trim().is_empty() { "{}" } else { arguments }) {
            Ok(Value::Object(map)) => Value::Object(map),
            _ => return format!("Error: the arguments aren't a JSON object: {}", clip(arguments, 200)),
        };
        let result = match name {
            "read" => self.read(&args),
            "glob" => self.glob(&args),
            "grep" => self.grep(&args),
            "write" if self.edits => self.write(&args),
            "edit" if self.edits => self.edit(&args),
            "bash" if self.runner.is_some() => self.bash(&args, cancel).await,
            _ => Err(format!("there is no tool called `{name}` here")),
        };
        match result {
            Ok(text) => clip(&text, MAX_RESULT),
            Err(e) => format!("Error: {e}"),
        }
    }

    /// `path` (relative to the main folder unless absolute) with symlinks resolved, if the
    /// project lets the agent use it so.
    fn resolve(&self, path: &str, access: Access) -> Result<PathBuf, String> {
        let path = path.trim();
        if path.is_empty() {
            return Err("no path given".into());
        }
        let mapped = self
            .path_mapper
            .as_ref()
            .and_then(|m| m.to_host(Path::new(path)));
        let path = match mapped {
            Some(host) => host,
            None => path.strip_prefix("~/").map_or_else(
                || PathBuf::from(path),
                |rest| directories::BaseDirs::new().map_or(PathBuf::from(path), |d| d.home_dir().join(rest)),
            ),
        };
        let joined = if path.is_absolute() { path } else { self.main().join(path) };
        let real = real_path(&joined).ok_or_else(|| format!("{}: can't resolve it", joined.display()))?;
        let folder = self
            .folders
            .iter()
            .filter(|f| real.starts_with(&f.path))
            .max_by_key(|f| f.path.as_os_str().len())
            .ok_or_else(|| format!("{} is outside the project's folders", real.display()))?;
        if access == Access::Write && !folder.writable {
            return Err(format!("{} is in a read-only folder of this project", real.display()));
        }
        Ok(real)
    }

    fn show(&self, path: &Path) -> String {
        match path.strip_prefix(self.main()) {
            Ok(rest) if rest.as_os_str().is_empty() => ".".into(),
            Ok(rest) => rest.display().to_string(),
            Err(_) => path.display().to_string(),
        }
    }

    fn read(&self, args: &Value) -> Result<String, String> {
        let path = self.resolve(str_arg(args, "path")?, Access::Read)?;
        if path.is_dir() {
            return self.list(&path);
        }
        let meta = std::fs::metadata(&path).map_err(|e| format!("{}: {e}", self.show(&path)))?;
        if !meta.is_file() {
            return Err(format!("{} is not a regular file", self.show(&path)));
        }
        if meta.len() > MAX_READ_FILE {
            return Err(format!("{} is too large to read ({} bytes)", self.show(&path), meta.len()));
        }
        let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", self.show(&path)))?;
        if bytes[..bytes.len().min(8000)].contains(&0) {
            return Ok(format!("{} is a binary file ({} bytes).", self.show(&path), bytes.len()));
        }
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        if lines.is_empty() {
            return Ok(format!("{} is empty.", self.show(&path)));
        }
        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map_or(MAX_READ_LINES, |l| (l as usize).clamp(1, MAX_READ_LINES));
        if offset > lines.len() {
            return Err(format!("{} has only {} lines", self.show(&path), lines.len()));
        }
        let end = (offset - 1 + limit).min(lines.len());
        let mut out: String = lines[offset - 1..end]
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{:>6}\t{}\n", offset + i, clip(line, MAX_LINE)))
            .collect();
        if offset > 1 || end < lines.len() {
            out.push_str(&format!(
                "({} has {} lines; this is {offset}–{end}. Use offset to read more.)\n",
                self.show(&path),
                lines.len()
            ));
        }
        Ok(out)
    }

    fn list(&self, dir: &Path) -> Result<String, String> {
        let mut entries: Vec<String> = std::fs::read_dir(dir)
            .map_err(|e| format!("{}: {e}", self.show(dir)))?
            .flatten()
            .map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if e.file_type().is_ok_and(|t| t.is_dir()) { format!("{name}/") } else { name }
            })
            .collect();
        entries.sort();
        let total = entries.len();
        entries.truncate(MAX_LISTED);
        let mut out = format!("{}/ ({total} entries)\n{}\n", self.show(dir), entries.join("\n"));
        if total > MAX_LISTED {
            out.push_str(&format!("({} more not shown)\n", total - MAX_LISTED));
        }
        Ok(out)
    }

    fn walk(&self, root: &Path, include: Option<&str>) -> Result<impl Iterator<Item = PathBuf>, String> {
        let mut builder = ignore::WalkBuilder::new(root);
        builder
            .hidden(false)
            .require_git(false)
            .filter_entry(|e| e.file_name() != ".git");
        if let Some(pattern) = include {
            let mut overrides = ignore::overrides::OverrideBuilder::new(root);
            overrides
                .add(pattern)
                .map_err(|e| format!("bad pattern {pattern}: {e}"))?;
            builder.overrides(overrides.build().map_err(|e| e.to_string())?);
        }
        Ok(builder
            .build()
            .flatten()
            .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
            .map(|e| e.into_path()))
    }

    fn glob(&self, args: &Value) -> Result<String, String> {
        let pattern = str_arg(args, "pattern")?;
        let root = self.resolve(args.get("path").and_then(Value::as_str).unwrap_or("."), Access::Read)?;
        let mut found: Vec<String> = self.walk(&root, Some(pattern))?.map(|p| self.show(&p)).collect();
        found.sort();
        if found.is_empty() {
            return Ok(format!("No files match {pattern}."));
        }
        let total = found.len();
        found.truncate(MAX_MATCHES);
        let mut out = found.join("\n") + "\n";
        if total > MAX_MATCHES {
            out.push_str(&format!("({} more; narrow the pattern)\n", total - MAX_MATCHES));
        }
        Ok(out)
    }

    fn grep(&self, args: &Value) -> Result<String, String> {
        let pattern = str_arg(args, "pattern")?;
        let regex = regex::Regex::new(pattern).map_err(|e| format!("bad regular expression: {e}"))?;
        let root = self.resolve(args.get("path").and_then(Value::as_str).unwrap_or("."), Access::Read)?;
        let files: Vec<PathBuf> = if root.is_file() {
            vec![root.clone()]
        } else {
            self.walk(&root, args.get("include").and_then(Value::as_str))?.collect()
        };
        let mut out = String::new();
        let mut matches = 0;
        for file in files {
            if std::fs::metadata(&file).map_or(true, |m| m.len() > MAX_SEARCHED_FILE) {
                continue;
            }
            let Ok(bytes) = std::fs::read(&file) else { continue };
            if bytes[..bytes.len().min(8000)].contains(&0) {
                continue;
            }
            for (i, line) in String::from_utf8_lossy(&bytes).lines().enumerate() {
                if regex.is_match(line) {
                    matches += 1;
                    if matches <= MAX_MATCHES {
                        out.push_str(&format!("{}:{}: {}\n", self.show(&file), i + 1, clip(line.trim(), 300)));
                    }
                }
            }
        }
        if matches == 0 {
            return Ok(format!("No matches for {pattern}."));
        }
        if matches > MAX_MATCHES {
            out.push_str(&format!("({} more matches; narrow the search)\n", matches - MAX_MATCHES));
        }
        Ok(out)
    }

    fn write(&self, args: &Value) -> Result<String, String> {
        let path = self.resolve(str_arg(args, "path")?, Access::Write)?;
        let content = str_arg(args, "content")?;
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(format!("{} is a symlink", self.show(&path)));
        }
        let existed = path.exists();
        let before = existed.then(|| std::fs::read_to_string(&path).ok()).flatten();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", self.show(parent)))?;
        }
        std::fs::write(&path, content).map_err(|e| format!("{}: {e}", self.show(&path)))?;
        let shown = self.show(&path);
        let mut msg = format!(
            "{} {} ({} lines).",
            if existed { "Replaced" } else { "Created" },
            shown,
            content.lines().count()
        );
        if let Some(prev) = before {
            let diff = edit::unified_diff(&shown, &prev, content, 2, 80);
            if !diff.is_empty() {
                msg.push_str("\n\n");
                msg.push_str(&diff);
            }
        }
        Ok(msg)
    }

    fn edit(&self, args: &Value) -> Result<String, String> {
        let path = self.resolve(str_arg(args, "path")?, Access::Write)?;
        let shown = self.show(&path);
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{shown}: {e}"))?;

        let outcome = if let Some(raw_edits) = args.get("edits") {
            let specs: Vec<EditSpec> = serde_json::from_value(raw_edits.clone())
                .map_err(|e| format!("invalid `edits` array: {e}"))?;
            edit::apply_multi_edit(&shown, &text, &specs)?
        } else {
            let old = str_arg(args, "old_string")?;
            let new = str_arg(args, "new_string")?;
            let all = args.get("replace_all").and_then(Value::as_bool).unwrap_or(false);
            edit::apply_edit(&shown, &text, old, new, all)?
        };

        std::fs::write(&path, &outcome.updated).map_err(|e| format!("{shown}: {e}"))?;
        let note = if outcome.strategy == edit::MatchStrategy::Exact {
            String::new()
        } else {
            format!(" [{}]", outcome.strategy.label())
        };
        let mut summary = format!(
            "Edited {shown} ({} replacement{}){note}.",
            outcome.replacements,
            if outcome.replacements == 1 { "" } else { "s" }
        );
        if !outcome.diff.is_empty() {
            summary.push_str("\n\n");
            summary.push_str(&outcome.diff);
        }
        Ok(summary)
    }

    async fn bash(&self, args: &Value, cancel: watch::Receiver<bool>) -> Result<String, String> {
        let command = str_arg(args, "command")?;
        let timeout = args
            .get("timeout")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT)
            .clamp(1, MAX_TIMEOUT);
        let runner = self
            .runner
            .as_ref()
            .ok_or("commands are turned off for this project")?;
        let output = runner
            .run(command, Duration::from_secs(timeout), cancel)
            .await?;
        Ok(output.format())
    }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("`{key}` is missing"))
}

/// `path` with symlinks resolved: its nearest existing ancestor resolved, the rest added.
/// Dangling symlinks are followed iteratively so a link inside the project cannot point
/// outside it; symlink loops and unresolved `..` climbs return `None`.
fn real_path(path: &Path) -> Option<PathBuf> {
    const MAX_SYMLINKS: usize = 40;
    let mut current = path.to_path_buf();
    let mut hops = 0usize;
    'resolve: loop {
        let mut existing = current.clone();
        let mut rest = Vec::new();
        loop {
            if let Ok(resolved) = std::fs::canonicalize(&existing) {
                return Some(rest.into_iter().rev().fold(resolved, |p: PathBuf, part| p.join(part)));
            }
            if std::fs::symlink_metadata(&existing).is_ok_and(|m| m.file_type().is_symlink()) {
                hops += 1;
                if hops > MAX_SYMLINKS {
                    return None;
                }
                let target = std::fs::read_link(&existing).ok()?;
                let base = if target.is_absolute() {
                    target
                } else {
                    let parent = existing.parent()?;
                    let resolved_parent = std::fs::canonicalize(parent).ok()?;
                    lexical_join(&resolved_parent, &target)?
                };
                current = rest.into_iter().rev().fold(base, |p: PathBuf, part| p.join(part));
                continue 'resolve;
            }
            match existing.components().next_back()? {
                std::path::Component::Normal(name) => rest.push(name.to_owned()),
                std::path::Component::CurDir => {}
                _ => return None,
            }
            existing = existing.parent()?.to_path_buf();
        }
    }
}

fn lexical_join(base: &Path, rel: &Path) -> Option<PathBuf> {
    let mut out = base.to_path_buf();
    for component in rel.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            std::path::Component::Normal(part) => out.push(part),
            _ => return None,
        }
    }
    Some(out)
}

/// `text` cut to about `max` characters, keeping its start and end.
pub fn clip(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max * 2 / 3).collect();
    let tail: String = text.chars().skip(count - max / 3).collect();
    format!(
        "{head}\n… [{} characters cut] …\n{tail}",
        count - head.chars().count() - tail.chars().count()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(name: &str) -> (PathBuf, Workspace) {
        let root = std::env::temp_dir().join(format!("entropy-tools-{name}-{}", std::process::id()));
        let main = root.join("app");
        let docs = root.join("docs");
        std::fs::create_dir_all(main.join("src")).unwrap();
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(main.join("src/lib.rs"), "fn one() {}\nfn two() {}\n").unwrap();
        std::fs::write(main.join(".gitignore"), "target/\n").unwrap();
        std::fs::create_dir_all(main.join("target")).unwrap();
        std::fs::write(main.join("target/out.rs"), "fn one() {}\n").unwrap();
        std::fs::write(docs.join("guide.md"), "# Guide\n").unwrap();
        std::fs::write(root.join("secret.txt"), "key").unwrap();
        let folders = [
            WorkspaceFolder { path: main, writable: true },
            WorkspaceFolder { path: docs, writable: false },
        ];
        (root.clone(), Workspace::new(&folders, None, true).unwrap())
    }

    async fn run(ws: &Workspace, name: &str, args: Value) -> String {
        ws.run(name, &args.to_string(), watch::channel(false).1).await
    }

    #[tokio::test]
    async fn reads_only_inside_the_project() {
        let (root, ws) = workspace("read");
        assert!(run(&ws, "read", json!({"path": "src/lib.rs"})).await.contains("     2\tfn two() {}"));
        assert!(run(&ws, "read", json!({"path": "src/lib.rs", "offset": 2})).await.contains("this is 2–2"));
        assert!(run(&ws, "read", json!({"path": root.join("docs/guide.md")})).await.contains("# Guide"));
        assert!(run(&ws, "read", json!({"path": "../secret.txt"})).await.contains("outside the project's folders"));
        assert!(run(&ws, "read", json!({"path": "src"})).await.contains("lib.rs"));
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn changes_only_writable_folders() {
        let (root, ws) = workspace("write");
        assert!(
            run(&ws, "write", json!({"path": "src/new/mod.rs", "content": "a\nb\n"}))
                .await
                .starts_with("Created src/new/mod.rs (2 lines)")
        );
        assert!(run(&ws, "write", json!({"path": root.join("docs/x.md"), "content": "x"})).await.contains("read-only folder"));
        assert!(run(&ws, "write", json!({"path": "../escape.txt", "content": "x"})).await.contains("outside"));
        assert!(!root.join("escape.txt").exists());
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn edits_exact_multi_and_returns_diff() {
        let (root, ws) = workspace("edit");
        let edit = |old: &str, new: &str, all: bool| {
            json!({"path": "src/lib.rs", "old_string": old, "new_string": new, "replace_all": all})
        };
        assert!(run(&ws, "edit", edit("fn ", "pub fn ", false)).await.contains("appears 2 times"));
        let res = run(&ws, "edit", edit("fn two", "fn second", false)).await;
        assert!(res.starts_with("Edited src/lib.rs (1 replacement)."), "{res}");
        assert!(res.contains("-fn two() {}") && res.contains("+fn second() {}"), "{res}");

        let multi = run(
            &ws,
            "edit",
            json!({
                "path": "src/lib.rs",
                "edits": [
                    {"old_string": "fn one()", "new_string": "pub fn one()"},
                    {"old_string": "fn second()", "new_string": "pub fn second()"}
                ]
            }),
        )
        .await;
        assert!(multi.starts_with("Edited src/lib.rs (2 replacements)."), "{multi}");
        assert_eq!(
            std::fs::read_to_string(root.join("app/src/lib.rs")).unwrap(),
            "pub fn one() {}\npub fn second() {}\n"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn searches_skip_ignored_files() {
        let (root, ws) = workspace("search");
        assert_eq!(run(&ws, "glob", json!({"pattern": "*.rs"})).await, "src/lib.rs\n");
        assert_eq!(run(&ws, "grep", json!({"pattern": "fn one"})).await, "src/lib.rs:1: fn one() {}\n");
        assert!(run(&ws, "grep", json!({"pattern": "Guide", "path": root.join("docs")})).await.contains("guide.md:1: # Guide"));
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn refuses_what_the_project_turned_off() {
        let (root, _) = workspace("off");
        let ws = Workspace::new(&[WorkspaceFolder { path: root.join("app"), writable: true }], None, false).unwrap();
        let names: Vec<String> = ws.definitions().iter().map(|t| t["function"]["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(names, ["read", "glob", "grep"]);
        assert!(run(&ws, "bash", json!({"command": "ls"})).await.contains("no tool called `bash`"));
        assert!(run(&ws, "write", json!({"path": "x", "content": ""})).await.contains("no tool called"));
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dangling_symlinks_cannot_escape_project_on_write() {
        let (root, ws) = workspace("symlink-escape");
        let outside = root.join("outside-new.txt");
        let abs_link = root.join("app/abs-link");
        let rel_link = root.join("app/rel-link");
        std::os::unix::fs::symlink(&outside, &abs_link).unwrap();
        std::os::unix::fs::symlink("../outside-rel.txt", &rel_link).unwrap();
        assert!(run(&ws, "write", json!({"path": "abs-link", "content": "pwn"})).await.starts_with("Error:"));
        assert!(!outside.exists());
        assert!(run(&ws, "write", json!({"path": "rel-link", "content": "pwn"})).await.starts_with("Error:"));
        assert!(!root.join("outside-rel.txt").exists());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn clips_long_text_at_both_ends() {
        let text = "a".repeat(100) + &"b".repeat(100);
        let clipped = clip(&text, 30);
        assert!(clipped.starts_with("aaaaaaaaaaaaaaaaaaaa\n…") && clipped.ends_with("bbbbbbbbbb"), "{clipped}");
    }
}
