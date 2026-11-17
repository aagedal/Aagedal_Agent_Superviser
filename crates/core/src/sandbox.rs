//! A macOS sandbox (Seatbelt) for the coding agent, enforced by the kernel and inherited
//! by every process it starts. Where the guard reads what the agent *tries* to do, this
//! limits what it *can* do, however the attempt is phrased:
//!
//! - writes only in the project, temporary folders and tool caches;
//! - no reading credentials (SSH, cloud, registry and git tokens, keychains, browser
//!   profiles, shell history), other agents' histories or this app's state;
//! - no escaping through processes the sandbox doesn't cover: apps opened through
//!   LaunchServices (`open`), AppleScript, `launchctl` jobs, the keychain CLI, Docker's
//!   socket;
//! - optionally, no network beyond this machine.
//!
//! Profiles use `sandbox-exec`, which Apple marks deprecated but which macOS still ships and
//! enforces; the last matching rule wins.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::paths::{Paths, write_private};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Network {
    /// Package installs and git work; the guard still checks where calls send data.
    #[default]
    Internet,
    /// Only this machine (the guard, local servers). Installs and clones fail.
    LocalOnly,
}

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
            network,
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
        let blocked: Vec<PathBuf> = BLOCKED_PROGRAMS.iter().map(PathBuf::from).collect();
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

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// What [`launch_opencode`] wrote and started.
#[derive(Debug, Clone, Serialize)]
pub struct Launched {
    pub profile: PathBuf,
    pub script: PathBuf,
}

/// A script that opens OpenCode in `project`, inside the sandbox, with a clean environment.
pub fn launch_script(policy: &Policy, profile: &Path, opencode: &Path) -> String {
    let kept: String = KEPT_ENVIRONMENT.iter().map(|k| format!(" {k}=\"${k}\"")).collect();
    let name = policy.project.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let network = match policy.network {
        Network::Internet => "Internet: on (the guard still checks every call).",
        Network::LocalOnly => "Internet: off (only this machine).",
    };
    format!(
        r#"#!/bin/zsh -l
# Generated by Agent Superviser: OpenCode in a sandbox.
cd {project} || exit 1
printf '\033]0;OpenCode (sandboxed): %s\007' {name}
echo "Agent Superviser: OpenCode runs in a sandbox. It can change files only in"
echo "  {project_display}"
echo "and can't read your SSH keys, cloud credentials, keychains, browser data or other agents' history."
echo "{network}"
echo
exec /usr/bin/env -i{kept} /usr/bin/sandbox-exec -f {profile} {opencode} --standalone
"#,
        project = shell_quote(&policy.project.display().to_string()),
        project_display = policy.project.display().to_string().replace('"', ""),
        name = shell_quote(&name),
        profile = shell_quote(&profile.display().to_string()),
        opencode = shell_quote(&opencode.display().to_string()),
    )
}

/// Opens OpenCode for `project` in a new Terminal window, sandboxed. macOS only.
pub fn launch_opencode(paths: &Paths, project: &Path, network: Network) -> anyhow::Result<Launched> {
    anyhow::ensure!(cfg!(target_os = "macos"), "the sandbox is only available on macOS so far");
    anyhow::ensure!(project.is_dir(), "{} is not a folder", project.display());
    let opencode = which::which("opencode").map_err(|_| anyhow::anyhow!("OpenCode was not found; install it from opencode.ai"))?;
    let policy = Policy::new(project, paths, network)?;
    anyhow::ensure!(
        !(policy.home.starts_with(&policy.project)),
        "{} contains your home folder; pick a project folder inside it",
        project.display()
    );
    let dir = paths.data.join("sandbox");
    let stem = format!("{:x}", fnv(policy.project.display().to_string().as_bytes()));
    let profile = dir.join(format!("{stem}.sb"));
    let script = dir.join(format!("open-{stem}.command"));
    write_private(&profile, policy.profile().as_bytes())?;
    write_private(&script, launch_script(&policy, &profile, &opencode).as_bytes())?;
    #[cfg(unix)]
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    let status = std::process::Command::new("/usr/bin/open").args(["-a", "Terminal"]).arg(&script).status()?;
    anyhow::ensure!(status.success(), "Terminal didn't open");
    Ok(Launched { profile, script })
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3))
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
        let script = launch_script(&policy, Path::new("/x/p.sb"), Path::new("/bin/opencode"));
        assert!(script.contains(r#"cd '/Users/a/my "odd" app'"#));
        assert!(script.contains("/usr/bin/env -i HOME=\"$HOME\""));
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
        let mut policy = Policy::for_home(&project, &home, &temp, Network::LocalOnly);
        policy.hidden.push(home.join("state"));
        policy.readable.push(home.join("state/keys/local-api-key"));

        let probe = |label: &str, command: &str| format!("if {command} >/dev/null 2>&1; then echo '{label}=yes'; else echo '{label}=no'; fi\n");
        let script = [
            probe("write_project", "sh -c 'echo x > made.txt'"),
            probe("write_home", &format!("sh -c 'echo x > {}/made.txt'", home.display())),
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
            "write_project=yes", "write_home=no", "write_cache=yes", "write_temp=yes", "read_ssh=no", "read_guard_key=yes",
            "read_upstream_key=no", "open_app=no", "osascript=no", "internet=no",
        ];
        for line in expect {
            assert!(out.contains(line), "expected {line} in:\n{out}");
        }
        std::fs::remove_dir_all(root).ok();
    }
}
