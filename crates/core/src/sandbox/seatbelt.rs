//! The macOS sandbox backend (Seatbelt), enforced by the kernel and inherited by every
//! process the agent starts. Where the guard reads what the agent *tries* to do, this
//! limits what it *can* do, however the attempt is phrased:
//!
//! - writes only in the project's writable folders, temporary folders and tool caches;
//! - no reading credentials (SSH, cloud, registry and git tokens, keychains, browser
//!   profiles, shell history), other agents' histories or this app's state;
//! - no escaping through processes the sandbox doesn't cover: apps opened through
//!   LaunchServices (`open`), AppleScript, `launchctl` jobs, the keychain CLI, Docker's
//!   socket, and the programs a project blocks;
//! - optionally, no network beyond this machine.
//!
//! Profiles use `sandbox-exec`, which Apple marks deprecated but which macOS still ships and
//! enforces; the last matching rule wins.

use std::path::{Path, PathBuf};

use crate::opencode::PROVIDER_ID;
use crate::paths::{Paths, write_private};
use crate::project::Folder;

use super::{Backend, Isolation, Network, Prepared, Request, shell_quote};

#[derive(Debug, Clone)]
pub struct Policy {
    pub project: PathBuf,
    pub home: PathBuf,
    /// The per-user temporary root (`$TMPDIR/..`, under `/private/var/folders`).
    pub temp_root: PathBuf,
    /// Writable besides the project and temporary folders.
    pub writable: Vec<PathBuf>,
    /// Neither readable nor writable.
    pub hidden: Vec<PathBuf>,
    /// Readable even inside a hidden folder (the key OpenCode sends the guard).
    pub readable: Vec<PathBuf>,
    /// Programs that may not run, besides the ones every project blocks.
    pub blocked: Vec<PathBuf>,
    pub network: Network,
}

/// Credentials and private data under the home folder.
const HIDDEN_IN_HOME: &[&str] = &[
    ".ssh", ".gnupg", ".aws", ".azure", ".kube", ".docker/config.json", ".netrc", ".npmrc", ".pypirc", ".git-credentials",
    ".config/git/credentials", ".config/gh", ".config/gcloud", ".config/hub", ".terraform.d/credentials.tfrc.json",
    ".cache/huggingface/token", ".huggingface/token", ".zsh_history", ".bash_history", ".python_history", ".node_repl_history",
    // Other agents' sessions and settings.
    ".claude", ".claude.json", ".codex", ".cursor", ".gemini", ".openhands",
    // Keychains, cookies, mail and messages, browser profiles.
    "Library/Keychains", "Library/Cookies", "Library/Mail", "Library/Messages", "Library/Safari",
    "Library/Application Support/Google/Chrome", "Library/Application Support/Firefox", "Library/Application Support/BraveSoftware",
    "Library/Application Support/Arc", "Library/Application Support/Microsoft Edge", "Library/Application Support/com.apple.TCC",
];

/// Caches and stores package managers and build tools write to.
const WRITABLE_IN_HOME: &[&str] = &[
    // OpenCode's own data, state and cache (`opencode debug paths`).
    ".local/share/opencode", ".local/state/opencode", ".cache",
    "Library/Caches", ".npm", ".bun", ".pnpm-store", "Library/pnpm", ".yarn", ".cargo/registry", ".cargo/git", "go/pkg/mod",
    ".gradle", ".m2/repository", ".local/share/uv", ".deno",
];

/// Programs that start processes outside the sandbox, or read the keychain.
const BLOCKED_PROGRAMS: &[&str] = &["/usr/bin/open", "/usr/bin/osascript", "/bin/launchctl", "/usr/bin/security"];

/// Docker's sockets: talking to the daemon can start a container that mounts anything.
const DOCKER_SOCKETS_IN_HOME: &[&str] = &[".docker/run/docker.sock", ".colima/default/docker.sock", ".orbstack/run/docker.sock", ".rd/docker.sock"];

/// `path` with symlinks resolved when it exists (Seatbelt matches real paths): `/tmp` is
/// `/private/tmp`, `/var` is `/private/var`.
fn real(path: &Path) -> PathBuf {
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return resolved;
    }
    // Resolve the nearest existing ancestor and keep the rest.
    let mut rest = Vec::new();
    let mut current = path.to_path_buf();
    while let Some(parent) = current.parent().map(Path::to_path_buf) {
        rest.push(current.file_name().map(|n| n.to_owned()).unwrap_or_default());
        if let Ok(resolved) = std::fs::canonicalize(&parent) {
            return rest.into_iter().rev().fold(resolved, |p, n| p.join(n));
        }
        current = parent;
    }
    path.to_path_buf()
}

impl Policy {
    /// The default policy for `project`: the app's state hidden except the guard key.
    pub fn new(project: &Path, paths: &Paths, network: Network) -> anyhow::Result<Self> {
        let home = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()).ok_or_else(|| anyhow::anyhow!("no home folder"))?;
        let temp = std::env::temp_dir();
        let temp_root = real(&temp).parent().map(Path::to_path_buf).unwrap_or(real(&temp));
        let mut policy = Self::for_home(project, &home, &temp_root, network);
        policy.hidden.push(real(&paths.data));
        policy.readable.push(real(&paths.local_api_key()));
        Ok(policy)
    }

    /// The default rules for a given home and temporary root.
    pub fn for_home(project: &Path, home: &Path, temp_root: &Path, network: Network) -> Self {
        Self {
            project: real(project),
            home: real(home),
            temp_root: real(temp_root),
            writable: WRITABLE_IN_HOME.iter().map(|p| real(&home.join(p))).collect(),
            hidden: HIDDEN_IN_HOME.iter().map(|p| real(&home.join(p))).collect(),
            readable: Vec::new(),
            blocked: Vec::new(),
            network,
        }
    }

    /// Also lets the agent change `folders`.
    pub fn write_also(&mut self, folders: impl IntoIterator<Item = PathBuf>) {
        self.writable.extend(folders.into_iter().map(|f| real(&f)));
    }

    /// Also blocks the programs called `names`, wherever they're installed.
    pub fn block_programs(&mut self, names: &[String]) {
        for name in names {
            self.blocked.extend(program_paths(name));
        }
    }

    /// The Seatbelt profile.
    pub fn profile(&self) -> String {
        let path = |p: &Path| format!("\"{}\"", p.display().to_string().replace('\\', "\\\\").replace('"', "\\\""));
        let subpaths = |ps: &[PathBuf]| ps.iter().map(|p| format!("\n  (subpath {})", path(p))).collect::<String>();
        let literals = |ps: &[PathBuf]| ps.iter().map(|p| format!("\n  (literal {})", path(p))).collect::<String>();
        let mut writable = vec![self.project.clone(), self.temp_root.clone(), PathBuf::from("/private/tmp"), PathBuf::from("/dev")];
        writable.extend(self.writable.iter().cloned());
        let mut out = String::from(";; Generated by Agent Superviser. The last matching rule wins.\n(version 1)\n(allow default)\n");
        out.push_str(&format!("\n;; Writes only where builds and package managers need to.\n(deny file-write*)\n(allow file-write*{})\n", subpaths(&writable)));
        out.push_str(&format!("\n;; Credentials, other agents' sessions and private data: unreadable and unwritable.\n(deny file-read* file-write*{})\n", subpaths(&self.hidden)));
        if !self.readable.is_empty() {
            out.push_str(&format!("(allow file-read*{})\n", literals(&self.readable)));
        }
        let mut blocked: Vec<PathBuf> = BLOCKED_PROGRAMS.iter().map(PathBuf::from).collect();
        blocked.extend(self.blocked.iter().cloned());
        out.push_str(&format!(
            "\n;; Ways to start processes outside the sandbox, and the keychain CLI.\n(deny process-exec{})\n(deny appleevent-send)\n(deny mach-lookup (global-name \"com.apple.coreservices.launchservicesd\"))\n",
            literals(&blocked)
        ));
        let mut sockets = vec![PathBuf::from("/private/var/run/docker.sock")];
        sockets.extend(DOCKER_SOCKETS_IN_HOME.iter().map(|p| self.home.join(p)));
        out.push_str(&format!(
            "\n;; Docker's daemon can run a container that mounts anything.\n(deny network-outbound{})\n",
            sockets.iter().map(|p| format!("\n  (remote unix-socket (path-literal {}))", path(p))).collect::<String>()
        ));
        if self.network == Network::LocalOnly {
            out.push_str("\n;; Only this machine.\n(deny network-outbound (remote ip \"*:*\"))\n(allow network-outbound (remote ip \"localhost:*\"))\n");
        }
        out
    }
}

/// Environment variables passed into the sandbox; everything else (API keys exported in a
/// shell profile, cloud credentials) stays out.
pub const KEPT_ENVIRONMENT: &[&str] = &["HOME", "USER", "LOGNAME", "SHELL", "PATH", "TERM", "TERM_PROGRAM", "COLORTERM", "LANG", "LC_ALL", "LC_CTYPE", "TMPDIR"];

/// Writes the profile for a project (its writable folders, blocked programs and network)
/// as `sandbox/<stem>.sb`, and returns its path.
pub fn write_project_profile(paths: &Paths, main: &Path, related: &[Folder], network: Network, blocked_programs: &[String], stem: &str) -> anyhow::Result<PathBuf> {
    let mut policy = Policy::new(main, paths, network)?;
    policy.write_also(related.iter().filter(|f| f.writable).map(|f| f.path.clone()));
    policy.block_programs(blocked_programs);
    let profile = paths.data.join("sandbox").join(format!("{stem}.sb"));
    write_private(&profile, policy.profile().as_bytes())?;
    Ok(profile)
}

/// The `PATH` of a login shell, where Homebrew, nvm, cargo and the like add themselves. The
/// app's own is short when it starts from Finder.
pub fn login_path() -> String {
    static PATH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        let shell = std::env::var("SHELL").ok().filter(|s| s.starts_with('/')).unwrap_or_else(|| "/bin/zsh".into());
        std::process::Command::new(shell)
            .args(["-l", "-c", "printf %s \"$PATH\""])
            .stdin(std::process::Stdio::null())
            .output()
            .ok()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".into())
    })
    .clone()
}

/// A shell running `script` in `profile`, from `cwd`, with the same reduced environment
/// OpenCode gets (and the login shell's `PATH`). Its errors go to its output.
pub fn shell(profile: &Path, cwd: &Path, script: &str) -> std::process::Command {
    sandboxed(profile, cwd, Path::new("/bin/zsh"), &["-c", &format!("exec 2>&1\n{script}")])
}

/// `program` with `args` in `profile`, from `cwd`, with only [`KEPT_ENVIRONMENT`] and the
/// login shell's `PATH` (bundled programs first).
pub fn sandboxed(profile: &Path, cwd: &Path, program: &Path, args: &[&str]) -> std::process::Command {
    let mut command = std::process::Command::new("/usr/bin/sandbox-exec");
    command.arg("-f").arg(profile).arg(program).args(args).current_dir(cwd).env_clear();
    for key in KEPT_ENVIRONMENT {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command.env("PATH", sandbox_path(&login_path()));
    command
}

/// `path` with the app's bundled programs first, so OpenCode finds the bundled `rg`.
pub fn sandbox_path(path: &str) -> String {
    match crate::paths::bundled("rg").and_then(|rg| rg.parent().map(|d| d.display().to_string())) {
        Some(dir) => format!("{dir}:{path}"),
        None => path.to_string(),
    }
}

/// Where programs are looked for besides `PATH` (which is short when the app starts from Finder).
const PROGRAM_DIRS: &[&str] = &["/usr/bin", "/bin", "/usr/sbin", "/sbin", "/usr/local/bin", "/opt/homebrew/bin", "/opt/homebrew/sbin"];

/// Every installed copy of the program `name`, with symlinks also resolved (Seatbelt
/// matches the file that runs).
fn program_paths(name: &str) -> Vec<PathBuf> {
    if name.is_empty() || name.contains('/') {
        return Vec::new();
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut found: Vec<PathBuf> = Vec::new();
    for dir in std::env::split_paths(&path).chain(PROGRAM_DIRS.iter().map(PathBuf::from)) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            for p in [candidate.clone(), real(&candidate)] {
                if !found.contains(&p) {
                    found.push(p);
                }
            }
        }
    }
    found
}

/// The sandbox command for a written profile: a clean environment, then OpenCode, given
/// `config` as `OPENCODE_CONFIG_CONTENT` when there is one.
pub fn command(profile: &Path, opencode: &Path, config: Option<&str>) -> String {
    let mut kept: String = KEPT_ENVIRONMENT
        .iter()
        .map(|k| if *k == "PATH" { format!(" PATH={}", shell_quote(&sandbox_path("$PATH")).replace("$PATH", "'\"$PATH\"'")) } else { format!(" {k}=\"${k}\"") })
        .collect();
    if let Some(config) = config {
        kept.push_str(&format!(" OPENCODE_CONFIG_CONTENT={}", shell_quote(config)));
    }
    format!(
        "exec /usr/bin/env -i{kept} /usr/bin/sandbox-exec -f {} {} --standalone",
        shell_quote(&profile.display().to_string()),
        shell_quote(&opencode.display().to_string())
    )
}

/// OpenCode in a Seatbelt profile, on the Mac's own tools.
pub struct Seatbelt;

impl Backend for Seatbelt {
    fn isolation(&self) -> Isolation {
        Isolation::MacosSandbox
    }

    fn label(&self) -> &'static str {
        "macOS sandbox"
    }

    fn available(&self) -> Result<(), String> {
        if !cfg!(target_os = "macos") {
            return Err("only on macOS".into());
        }
        if !Path::new("/usr/bin/sandbox-exec").exists() {
            return Err("this macOS has no sandbox-exec".into());
        }
        Ok(())
    }

    fn prepare(&self, request: &Request, stem: &str) -> anyhow::Result<Prepared> {
        let opencode = crate::opencode::binary().ok_or_else(|| anyhow::anyhow!("OpenCode was not found; install it from opencode.ai"))?;
        let profile = write_project_profile(request.paths, request.project, request.related, request.network, request.blocked_programs, stem)?;
        // The project's rules, and its path on the guard: OpenCode's global config already
        // has our provider, so only its address changes.
        let mut config = request.opencode.clone();
        config["provider"] = serde_json::json!({ PROVIDER_ID: { "options": { "baseURL": format!("{}{}/v1", request.guard_url, request.guard_path) } } });
        let folders = if request.related.iter().any(|f| f.writable) { "the project's folders" } else { "this folder" };
        Ok(Prepared {
            banner: vec![
                format!("OpenCode runs in a macOS sandbox. It can change files only in {folders},"),
                "and can't read your SSH keys, cloud credentials, keychains, browser data or other agents' history.".into(),
            ],
            command: command(&profile, &opencode, Some(&config.to_string())),
            files: vec![profile],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_shape() {
        let policy = Policy::for_home(Path::new("/Users/a/code/app"), Path::new("/Users/a"), Path::new("/private/var/folders/xy/abc"), Network::LocalOnly);
        let profile = policy.profile();
        assert!(profile.contains("(subpath \"/Users/a/code/app\")"));
        assert!(profile.contains("(subpath \"/Users/a/.ssh\")"));
        assert!(profile.contains("(literal \"/usr/bin/open\")"));
        assert!(profile.contains("(allow network-outbound (remote ip \"localhost:*\"))"));
        // Hidden paths come after the writable ones, so they win.
        assert!(profile.find("(deny file-read* file-write*").unwrap() > profile.find("(allow file-write*").unwrap());
        assert!(!Policy::for_home(Path::new("/p"), Path::new("/h"), Path::new("/t"), Network::Internet).profile().contains("remote ip"));
    }

    #[test]
    fn quotes_in_paths() {
        let policy = Policy::for_home(Path::new("/Users/a/my \"odd\" app"), Path::new("/Users/a"), Path::new("/t"), Network::Internet);
        assert!(policy.profile().contains(r#"(subpath "/Users/a/my \"odd\" app")"#));
        let command = command(Path::new("/x/my 'p'.sb"), Path::new("/bin/opencode"), Some(r#"{"a":"it's"}"#));
        assert!(command.contains(r#"-f '/x/my '\''p'\''.sb'"#), "{command}");
        assert!(command.contains(r#"OPENCODE_CONFIG_CONTENT='{"a":"it'\''s"}'"#), "{command}");
        assert!(command.contains("/usr/bin/env -i HOME=\"$HOME\""));
    }

    /// Runs `script` in the sandbox and returns what it printed.
    #[cfg(target_os = "macos")]
    fn run(policy: &Policy, script: &str) -> String {
        let profile = policy.temp_root.join("test.sb");
        std::fs::write(&profile, policy.profile()).unwrap();
        let out = std::process::Command::new("/usr/bin/sandbox-exec")
            .arg("-f")
            .arg(&profile)
            .args(["/bin/sh", "-c", script])
            .current_dir(&policy.project)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// The kernel enforces the profile on the agent and every process it starts.
    #[cfg(target_os = "macos")]
    #[test]
    fn enforced_on_child_processes() {
        let root = real(&std::env::temp_dir()).join(format!("superviser-sandbox-{}", std::process::id()));
        let (home, temp, project) = (root.join("home"), root.join("tmp"), root.join("home/code/app"));
        for dir in [&home.join(".ssh"), &home.join(".cache"), &home.join("state/keys"), &temp, &project] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(home.join(".ssh/id_ed25519"), "secret").unwrap();
        std::fs::write(home.join("state/keys/local-api-key"), "key").unwrap();
        std::fs::write(home.join("state/keys/upstream-api-key"), "upstream").unwrap();
        let docs = home.join("code/docs");
        std::fs::create_dir_all(&docs).unwrap();
        let mut policy = Policy::for_home(&project, &home, &temp, Network::LocalOnly);
        policy.write_also([docs.clone()]);
        policy.block_programs(&["uname".into()]);
        policy.hidden.push(home.join("state"));
        policy.readable.push(home.join("state/keys/local-api-key"));

        let probe = |label: &str, command: &str| format!("if {command} >/dev/null 2>&1; then echo '{label}=yes'; else echo '{label}=no'; fi\n");
        let script = [
            probe("write_project", "sh -c 'echo x > made.txt'"),
            probe("write_home", &format!("sh -c 'echo x > {}/made.txt'", home.display())),
            probe("write_related", &format!("sh -c 'echo x > {}/made.txt'", docs.display())),
            probe("blocked_program", "sh -c uname"),
            probe("other_program", "sh -c hostname"),
            probe("write_cache", &format!("sh -c 'echo x > {}/.cache/made.txt'", home.display())),
            probe("write_temp", &format!("sh -c 'echo x > {}/made.txt'", temp.display())),
            probe("read_ssh", &format!("sh -c 'cat {}/.ssh/id_ed25519'", home.display())),
            probe("read_guard_key", &format!("cat {}/state/keys/local-api-key", home.display())),
            probe("read_upstream_key", &format!("cat {}/state/keys/upstream-api-key", home.display())),
            probe("open_app", "/usr/bin/open -Ra Finder"),
            probe("osascript", "/usr/bin/osascript -e 1"),
            probe("internet", "/usr/bin/curl -s -m 5 -o /dev/null http://1.1.1.1/"),
        ]
        .concat();
        let out = run(&policy, &script);
        let expect = [
            "write_project=yes", "write_home=no", "write_related=yes", "blocked_program=no", "other_program=yes", "write_cache=yes", "write_temp=yes", "read_ssh=no", "read_guard_key=yes",
            "read_upstream_key=no", "open_app=no", "osascript=no", "internet=no",
        ];
        for line in expect {
            assert!(out.contains(line), "expected {line} in:\n{out}");
        }
        std::fs::remove_dir_all(root).ok();
    }
}
