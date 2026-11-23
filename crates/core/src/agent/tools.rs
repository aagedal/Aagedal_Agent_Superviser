//! The built-in agent's tools: reading, searching and changing the project's files, and
//! running commands in its macOS sandbox. Paths outside the project's folders are refused,
//! and writes go only to its writable folders, before the sandbox would refuse them anyway.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::sync::watch;

use crate::project::Folder;

/// Characters of a tool result the model reads; longer ones keep their start and end.
const MAX_RESULT: usize = 30_000;
const MAX_READ_LINES: usize = 2000;
const MAX_LINE: usize = 2000;
const MAX_MATCHES: usize = 200;
const MAX_LISTED: usize = 500;
const MAX_READ_FILE: u64 = 16 << 20;
const MAX_SEARCHED_FILE: u64 = 2 << 20;
/// Bytes of command output kept from its start and its end.
const OUTPUT_HEAD: usize = 64 << 10;
const OUTPUT_TAIL: usize = 64 << 10;
const DEFAULT_TIMEOUT: u64 = 120;
const MAX_TIMEOUT: u64 = 600;

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

/// The folders the agent works in, and how it may use them.
pub struct Workspace {
    /// With symlinks resolved; the first is the main folder.
    folders: Vec<Folder>,
    /// The Seatbelt profile commands run in; `None` when commands are off.
    profile: Option<PathBuf>,
    edits: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Access {
    Read,
    Write,
}

impl Workspace {
    /// `folders` must exist; the first is the main one.
    pub fn new(folders: &[Folder], profile: Option<PathBuf>, edits: bool) -> anyhow::Result<Self> {
        anyhow::ensure!(!folders.is_empty(), "a project needs a folder");
        let folders = folders
            .iter()
            .map(|f| Ok(Folder { path: std::fs::canonicalize(&f.path).map_err(|e| anyhow::anyhow!("{}: {e}", f.path.display()))?, writable: f.writable }))
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Self { folders, profile, edits })
    }

    pub fn main(&self) -> &Path {
        &self.folders[0].path
    }

    /// The tools to offer the model, in the OpenAI format.
    pub fn definitions(&self) -> Vec<Value> {
        let tool = |name: &str, description: &str, properties: Value, required: &[&str]| {
            json!({ "type": "function", "function": { "name": name, "description": description, "parameters": { "type": "object", "properties": properties, "required": required } } })
        };
        let mut tools = vec![
            tool("read", "Read a text file (numbered lines) or list a folder. Paths are relative to the main folder unless absolute.", json!({
                "path": { "type": "string" },
                "offset": { "type": "integer", "description": "First line to read, from 1" },
                "limit": { "type": "integer", "description": "Lines to read (default 2000)" }
            }), &["path"]),
            tool("glob", "Find files by name pattern, like `**/*.rs` or `src/**/test_*.py`. Skips what .gitignore ignores.", json!({
                "pattern": { "type": "string" },
                "path": { "type": "string", "description": "Folder to search (default: the main folder)" }
            }), &["pattern"]),
            tool("grep", "Search file contents with a regular expression. Returns `file:line: text`. Skips what .gitignore ignores.", json!({
                "pattern": { "type": "string", "description": "Regular expression; start with (?i) to ignore case" },
                "path": { "type": "string", "description": "File or folder to search (default: the main folder)" },
                "include": { "type": "string", "description": "Only files matching this glob, like `*.ts`" }
            }), &["pattern"]),
        ];
        if self.edits {
            tools.push(tool("write", "Create a file or replace all of it. For changes to an existing file, prefer `edit`.", json!({
                "path": { "type": "string" },
                "content": { "type": "string" }
            }), &["path", "content"]));
            tools.push(tool("edit", "Replace exact text in a file. `old_string` must appear exactly once unless `replace_all` is true; include enough surrounding lines to make it unique. Read the file first.", json!({
                "path": { "type": "string" },
                "old_string": { "type": "string" },
                "new_string": { "type": "string" },
                "replace_all": { "type": "boolean" }
            }), &["path", "old_string", "new_string"]));
        }
        if self.profile.is_some() {
            tools.push(tool("bash", "Run a shell command (zsh) from the main folder, in the project's sandbox. Use it to build, test, run git and the like. Output is cut when long.", json!({
                "command": { "type": "string" },
                "timeout": { "type": "integer", "description": "Seconds before it is stopped (default 120, at most 600)" }
            }), &["command"]));
        }
        tools
    }

    /// Runs a tool call; the text is what the model reads, errors included. `cancel`
    /// stops a running command.
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
            "bash" if self.profile.is_some() => self.bash(&args, cancel).await,
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
        let path = path.strip_prefix("~/").map_or_else(|| PathBuf::from(path), |rest| directories::BaseDirs::new().map_or(PathBuf::from(path), |d| d.home_dir().join(rest)));
        let joined = if path.is_absolute() { path } else { self.main().join(path) };
        let real = real_path(&joined).ok_or_else(|| format!("{}: can't resolve it", joined.display()))?;
        // The deepest folder holding it decides, so a writable folder can sit in a read-only one.
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

    /// `path` as the agent should see it: relative to the main folder when inside it.
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
        let limit = args.get("limit").and_then(Value::as_u64).map_or(MAX_READ_LINES, |l| (l as usize).clamp(1, MAX_READ_LINES));
        if offset > lines.len() {
            return Err(format!("{} has only {} lines", self.show(&path), lines.len()));
        }
        let end = (offset - 1 + limit).min(lines.len());
        let mut out: String = lines[offset - 1..end].iter().enumerate().map(|(i, line)| format!("{:>6}\t{}\n", offset + i, clip(line, MAX_LINE))).collect();
        if offset > 1 || end < lines.len() {
            out.push_str(&format!("({} has {} lines; this is {offset}–{end}. Use offset to read more.)\n", self.show(&path), lines.len()));
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

    /// Files under `root` that .gitignore doesn't hide, matching `include` when given.
    fn walk(&self, root: &Path, include: Option<&str>) -> Result<impl Iterator<Item = PathBuf>, String> {
        let mut builder = ignore::WalkBuilder::new(root);
        // .gitignore counts in folders that aren't git repositories too.
        builder.hidden(false).require_git(false).filter_entry(|e| e.file_name() != ".git");
        if let Some(pattern) = include {
            let mut overrides = ignore::overrides::OverrideBuilder::new(root);
            overrides.add(pattern).map_err(|e| format!("bad pattern {pattern}: {e}"))?;
            builder.overrides(overrides.build().map_err(|e| e.to_string())?);
        }
        Ok(builder.build().flatten().filter(|e| e.file_type().is_some_and(|t| t.is_file())).map(|e| e.into_path()))
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
        let files: Vec<PathBuf> = if root.is_file() { vec![root.clone()] } else { self.walk(&root, args.get("include").and_then(Value::as_str))?.collect() };
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
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", self.show(parent)))?;
        }
        std::fs::write(&path, content).map_err(|e| format!("{}: {e}", self.show(&path)))?;
        Ok(format!("{} {} ({} lines).", if existed { "Replaced" } else { "Created" }, self.show(&path), content.lines().count()))
    }

    fn edit(&self, args: &Value) -> Result<String, String> {
        let path = self.resolve(str_arg(args, "path")?, Access::Write)?;
        let old = str_arg(args, "old_string")?;
        let new = str_arg(args, "new_string")?;
        let all = args.get("replace_all").and_then(Value::as_bool).unwrap_or(false);
        if old.is_empty() {
            return Err("old_string is empty; use write to create a file".into());
        }
        if old == new {
            return Err("old_string and new_string are the same".into());
        }
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", self.show(&path)))?;
        let count = text.matches(old).count();
        match count {
            0 => return Err(format!("old_string isn't in {}; read it again and copy the text exactly", self.show(&path))),
            n if n > 1 && !all => return Err(format!("old_string appears {n} times in {}; add surrounding lines to pick one, or set replace_all", self.show(&path))),
            _ => {}
        }
        let updated = if all { text.replace(old, new) } else { text.replacen(old, new, 1) };
        std::fs::write(&path, updated).map_err(|e| format!("{}: {e}", self.show(&path)))?;
        Ok(format!("Edited {} ({} replacement{}).", self.show(&path), count, if count == 1 { "" } else { "s" }))
    }

    async fn bash(&self, args: &Value, mut cancel: watch::Receiver<bool>) -> Result<String, String> {
        let command = str_arg(args, "command")?;
        let timeout = args.get("timeout").and_then(Value::as_u64).unwrap_or(DEFAULT_TIMEOUT).clamp(1, MAX_TIMEOUT);
        let profile = self.profile.as_ref().ok_or("commands are turned off for this project")?;
        let mut std_command = crate::sandbox::seatbelt::shell(profile, self.main(), command);
        // Its own process group, so a timeout stops everything it started.
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
                if let Some(pid) = self.0 {
                    let _ = std::process::Command::new("/bin/kill").args(["-KILL", &format!("-{pid}")]).status();
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
            _ = tokio::time::sleep(Duration::from_secs(timeout)) => return Err(format!("the command was stopped after {timeout} s")),
            _ = cancel.wait_for(|c| *c) => return Err("the user stopped the command".to_string()),
        };
        let mut output = String::from_utf8_lossy(&head).into_owned();
        if !tail.is_empty() {
            let cut = total - head.len() - tail.len();
            if cut > 0 {
                output.push_str(&format!("\n… [{cut} bytes cut] …\n"));
            }
            output.push_str(&String::from_utf8_lossy(&tail.into_iter().collect::<Vec<u8>>()));
        }
        let code = match status {
            Ok(status) => status.code().map_or("killed by a signal".into(), |c| format!("exit code {c}")),
            Err(e) => format!("unknown exit ({e})"),
        };
        Ok(if output.trim().is_empty() { format!("(no output, {code})") } else { format!("{}\n({code})", output.trim_end()) })
    }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key).and_then(Value::as_str).ok_or_else(|| format!("`{key}` is missing"))
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
    format!("{head}\n… [{} characters cut] …\n{tail}", count - head.chars().count() - tail.chars().count())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(name: &str) -> (PathBuf, Workspace) {
        let root = std::env::temp_dir().join(format!("superviser-tools-{name}-{}", std::process::id()));
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
        let folders = [Folder { path: main, writable: true }, Folder { path: docs, writable: false }];
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
        assert!(run(&ws, "read", json!({"path": root.join("secret.txt")})).await.contains("outside"));
        assert!(run(&ws, "read", json!({"path": "src"})).await.contains("lib.rs"));
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn changes_only_writable_folders() {
        let (root, ws) = workspace("write");
        assert!(run(&ws, "write", json!({"path": "src/new/mod.rs", "content": "a\nb\n"})).await.starts_with("Created src/new/mod.rs (2 lines)"));
        assert!(run(&ws, "write", json!({"path": root.join("docs/x.md"), "content": "x"})).await.contains("read-only folder"));
        assert!(run(&ws, "write", json!({"path": "../escape.txt", "content": "x"})).await.contains("outside"));
        assert!(!root.join("escape.txt").exists());
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn edits_exact_text_once() {
        let (root, ws) = workspace("edit");
        let edit = |old: &str, new: &str, all: bool| json!({"path": "src/lib.rs", "old_string": old, "new_string": new, "replace_all": all});
        assert!(run(&ws, "edit", edit("fn ", "pub fn ", false)).await.contains("appears 2 times"));
        assert!(run(&ws, "edit", edit("fn three", "x", false)).await.contains("isn't in"));
        assert_eq!(run(&ws, "edit", edit("fn two", "fn second", false)).await, "Edited src/lib.rs (1 replacement).");
        assert_eq!(run(&ws, "edit", edit("fn ", "pub fn ", true)).await, "Edited src/lib.rs (2 replacements).");
        assert_eq!(std::fs::read_to_string(root.join("app/src/lib.rs")).unwrap(), "pub fn one() {}\npub fn second() {}\n");
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn searches_skip_ignored_files() {
        let (root, ws) = workspace("search");
        assert_eq!(run(&ws, "glob", json!({"pattern": "*.rs"})).await, "src/lib.rs\n");
        assert_eq!(run(&ws, "grep", json!({"pattern": "fn one"})).await, "src/lib.rs:1: fn one() {}\n");
        assert!(run(&ws, "grep", json!({"pattern": "Guide", "path": root.join("docs")})).await.contains("guide.md:1: # Guide"));
        assert!(run(&ws, "grep", json!({"pattern": "("})).await.starts_with("Error: bad regular expression"));
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn refuses_what_the_project_turned_off() {
        let (root, _) = workspace("off");
        let ws = Workspace::new(&[Folder { path: root.join("app"), writable: true }], None, false).unwrap();
        let names: Vec<String> = ws.definitions().iter().map(|t| t["function"]["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(names, ["read", "glob", "grep"]);
        assert!(run(&ws, "bash", json!({"command": "ls"})).await.contains("no tool called `bash`"));
        assert!(run(&ws, "write", json!({"path": "x", "content": ""})).await.contains("no tool called"));
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn commands_run_in_the_sandbox() {
        let (root, _) = workspace("bash");
        let paths = crate::paths::Paths::at(root.join("data"));
        let main = std::fs::canonicalize(root.join("app")).unwrap();
        let profile = crate::sandbox::seatbelt::write_project_profile(&paths, &main, &[], crate::sandbox::Network::LocalOnly, &["uname".into()], "test").unwrap();
        let ws = Workspace::new(&[Folder { path: main, writable: true }], Some(profile), true).unwrap();
        assert!(ws.definitions().iter().any(|t| t["function"]["name"] == "bash"));
        assert_eq!(run(&ws, "bash", json!({"command": "echo hi > made.txt && cat made.txt"})).await, "hi\n(exit code 0)");
        // The test's folders are under the temporary folder, which the sandbox lets anyone write.
        let outside = directories::BaseDirs::new().unwrap().home_dir().join(format!("superviser-test-{}.txt", std::process::id()));
        assert!(run(&ws, "bash", json!({"command": format!("echo x > {}", outside.display())})).await.contains("exit code 1"));
        assert!(!outside.exists());
        let blocked = run(&ws, "bash", json!({"command": "uname"})).await;
        assert!(!blocked.contains("Darwin") && !blocked.contains("exit code 0"), "{blocked}");
        assert!(run(&ws, "bash", json!({"command": "sleep 5", "timeout": 1})).await.contains("stopped after 1 s"));
        let (tx, rx) = watch::channel(false);
        let slow = ws.run("bash", r#"{"command": "sleep 5"}"#, rx);
        let stop = async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            tx.send(true).unwrap();
        };
        let (out, ()) = tokio::join!(slow, stop);
        assert!(out.contains("the user stopped"), "{out}");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn clips_long_text_at_both_ends() {
        let text = "a".repeat(100) + &"b".repeat(100);
        let clipped = clip(&text, 30);
        assert!(clipped.starts_with("aaaaaaaaaaaaaaaaaaaa\n…") && clipped.ends_with("bbbbbbbbbb"), "{clipped}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dangling_symlinks_cannot_escape_project_on_write() {
        let (root, ws) = workspace("symlink-escape");
        let outside = root.join("outside-new.txt");
        let abs_link = root.join("app/abs-link");
        let rel_link = root.join("app/rel-link");
        let loop_a = root.join("app/loop-a");
        let loop_b = root.join("app/loop-b");
        std::os::unix::fs::symlink(&outside, &abs_link).unwrap();
        std::os::unix::fs::symlink("../outside-rel.txt", &rel_link).unwrap();
        std::os::unix::fs::symlink(&loop_b, &loop_a).unwrap();
        std::os::unix::fs::symlink(&loop_a, &loop_b).unwrap();

        let err_abs = run(&ws, "write", json!({"path": "abs-link", "content": "pwn"})).await;
        assert!(err_abs.starts_with("Error:"), "{err_abs}");
        assert!(!outside.exists());

        let err_rel = run(&ws, "write", json!({"path": "rel-link", "content": "pwn"})).await;
        assert!(err_rel.starts_with("Error:"), "{err_rel}");
        assert!(!root.join("outside-rel.txt").exists());

        let err_loop = run(&ws, "write", json!({"path": "loop-a", "content": "pwn"})).await;
        assert!(err_loop.starts_with("Error:"), "{err_loop}");

        std::fs::remove_dir_all(root).ok();
    }
}
