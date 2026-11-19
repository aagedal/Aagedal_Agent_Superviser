//! Where the coding agent runs, isolated from the rest of the machine. Every backend opens
//! OpenCode for a project in a new Terminal window and enforces the same policy: the agent
//! changes only the project, can't reach your credentials, and talks to models only through
//! the guard. They differ in how:
//!
//! - [`seatbelt`]: the macOS sandbox, on the Mac's own tools (Xcode, Swift, Homebrew). The default.
//! - [`container`]: a Linux VM per project (Apple `container`), sharing only the project
//!   folder. Optional, and not for projects that need macOS tools.

pub mod container;
pub mod seatbelt;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::paths::{Paths, write_private};
use crate::project::{Folder, Project};

pub use seatbelt::Policy;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Network {
    /// Package installs and git work; the guard still checks where calls send data.
    #[default]
    Internet,
    /// Only this machine (the guard, local servers). Installs and clones fail.
    LocalOnly,
}

/// How a project is isolated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Isolation {
    #[default]
    MacosSandbox,
    LinuxVm,
}

pub struct Request<'a> {
    pub paths: &'a Paths,
    /// The main folder, where OpenCode starts.
    pub project: &'a Path,
    /// The project's other folders.
    pub related: &'a [Folder],
    pub network: Network,
    /// The guard's Unix socket, for backends that can't reach it on localhost.
    pub guard_socket: &'a Path,
    /// The running model, for backends that write OpenCode's config themselves.
    pub model: Option<crate::opencode::SetupModel>,
    /// The guard's address on this machine (`http://127.0.0.1:8787`).
    pub guard_url: &'a str,
    /// The project's path on the guard (`/projects/<id>`), before `/v1`.
    pub guard_path: &'a str,
    /// The project's OpenCode config, passed in `OPENCODE_CONFIG_CONTENT`.
    pub opencode: &'a Value,
    /// Programs the project's commands may not run.
    pub blocked_programs: &'a [String],
}

/// What to open: a project, with the model and guard OpenCode should use.
pub struct Launch<'a> {
    pub project: &'a Project,
    pub model: Option<crate::opencode::SetupModel>,
    /// The guard's address on this machine (`http://127.0.0.1:8787`).
    pub guard_url: String,
    /// [`Project::opencode_config`].
    pub opencode: Value,
}

/// What a backend set up for one launch.
pub struct Prepared {
    /// Lines shown above OpenCode: what the agent can and can't do.
    pub banner: Vec<String>,
    /// The shell command that starts OpenCode, run from the project folder.
    pub command: String,
    /// Files written for it (profiles, configs).
    pub files: Vec<PathBuf>,
}

pub trait Backend: Send + Sync {
    fn isolation(&self) -> Isolation;
    /// What the app calls it.
    fn label(&self) -> &'static str;
    /// Whether it can run on this machine, or why not.
    fn available(&self) -> Result<(), String>;
    /// How to get what it's missing, when it isn't available.
    fn setup(&self) -> Option<Setup> {
        None
    }
    /// Whether it suits `project`, or why not.
    fn suits(&self, _project: &Path) -> Result<(), String> {
        Ok(())
    }
    /// Writes what the launch needs (named after `stem`) and says how to start OpenCode.
    fn prepare(&self, request: &Request, stem: &str) -> anyhow::Result<Prepared>;
}

pub fn backend(isolation: Isolation) -> &'static dyn Backend {
    match isolation {
        Isolation::MacosSandbox => &seatbelt::Seatbelt,
        Isolation::LinuxVm => &container::LinuxVm,
    }
}

/// How to install what a backend is missing.
#[derive(Debug, Clone, Serialize)]
pub struct Setup {
    /// What to install, in a few words.
    pub needs: &'static str,
    /// Where to get it.
    pub url: &'static str,
    /// What to do after downloading it.
    pub steps: &'static [&'static str],
}

/// One way a project could be isolated, and why it can't be, if it can't.
#[derive(Debug, Clone, Serialize)]
pub struct Choice {
    pub isolation: Isolation,
    pub label: &'static str,
    /// `None` when it can be used for this project.
    pub unavailable: Option<String>,
    /// How to make it available, when installing something would.
    pub setup: Option<Setup>,
}

/// Every backend, with whether it can isolate `project` on this machine.
pub fn choices(project: &Path) -> Vec<Choice> {
    [Isolation::MacosSandbox, Isolation::LinuxVm]
        .into_iter()
        .map(|isolation| {
            let backend = backend(isolation);
            let available = backend.available();
            let setup = available.as_ref().err().and(backend.setup());
            let unavailable = available.and_then(|()| backend.suits(project)).err();
            Choice { isolation, label: backend.label(), unavailable, setup }
        })
        .collect()
}

pub(crate) fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3))
}

/// What a project's launch files (and its VM) are named after: its id and main folder.
pub fn stem(project_id: &str, main: &Path) -> String {
    format!("{:x}", fnv(format!("{project_id}\n{}", main.display()).as_bytes()))
}

/// What the Terminal window says about a project's other folders and blocked programs.
pub fn rules_banner(related: &[Folder], blocked_programs: &[String]) -> Vec<String> {
    let mut lines: Vec<String> = related
        .iter()
        .map(|f| format!("Also {}: {}", if f.writable { "changes" } else { "reads" }, f.path.display()))
        .collect();
    if !blocked_programs.is_empty() {
        lines.push(format!("Blocked programs: {}.", blocked_programs.join(", ")));
    }
    lines
}

/// The Terminal script: into the project, say how it is isolated, start OpenCode.
pub fn launch_script(project: &Path, label: &str, network: Network, prepared: &Prepared) -> String {
    let name = project.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let network = match network {
        Network::Internet => "Internet: on (the guard still checks every call).",
        Network::LocalOnly => "Internet: off (only this machine).",
    };
    let echo = |line: &str| format!("echo {}\n", shell_quote(line));
    let banner: String = prepared.banner.iter().map(|l| echo(l)).collect();
    format!(
        "#!/bin/zsh -l\n# Generated by Agent Superviser: OpenCode, isolated ({label}).\ncd {project} || exit 1\nprintf '\\033]0;OpenCode ({label}): %s\\007' {name}\necho {folder}\n{banner}{network}echo\n{command}\n",
        project = shell_quote(&project.display().to_string()),
        name = shell_quote(&name),
        folder = shell_quote(&format!("Agent Superviser: {}", project.display())),
        network = echo(network),
        command = prepared.command,
    )
}

/// What [`launch`] wrote and started.
#[derive(Debug, Clone, Serialize)]
pub struct Launched {
    pub script: PathBuf,
    pub files: Vec<PathBuf>,
}

/// A folder a project may use: an existing folder, not one holding the home folder.
fn checked_folder(path: &Path) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(path.is_dir(), "{} is not a folder", path.display());
    let path = std::fs::canonicalize(path)?;
    if let Some(home) = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()) {
        anyhow::ensure!(!home.starts_with(&path), "{} contains your home folder; pick a project folder inside it", path.display());
    }
    Ok(path)
}

/// Opens OpenCode for a project in a new Terminal window, isolated as the project says.
pub fn launch(paths: &Paths, launch: &Launch) -> anyhow::Result<Launched> {
    let project = launch.project;
    let main = checked_folder(project.main())?;
    let related = project
        .related()
        .iter()
        .map(|f| Ok(Folder { path: checked_folder(&f.path)?, writable: f.writable }))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let backend = backend(project.isolation);
    backend.available().map_err(|why| anyhow::anyhow!("{} isn't available: {why}", backend.label()))?;
    backend.suits(&main).map_err(|why| anyhow::anyhow!("{} doesn't suit this project: {why}", backend.label()))?;
    let stem = stem(&project.id, &main);
    let guard_socket = paths.guard_socket();
    let guard_path = project.guard_path();
    let request = Request {
        paths,
        project: &main,
        related: &related,
        network: project.network,
        guard_socket: &guard_socket,
        model: launch.model.clone(),
        guard_url: &launch.guard_url,
        guard_path: &guard_path,
        opencode: &launch.opencode,
        blocked_programs: &project.blocked_programs,
    };
    let mut prepared = backend.prepare(&request, &stem)?;
    prepared.banner.extend(rules_banner(&related, &project.blocked_programs));
    let script = paths.data.join("sandbox").join(format!("open-{stem}.command"));
    write_private(&script, launch_script(&main, backend.label(), project.network, &prepared).as_bytes())?;
    #[cfg(unix)]
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    let status = std::process::Command::new("/usr/bin/open").args(["-a", "Terminal"]).arg(&script).status()?;
    anyhow::ensure!(status.success(), "Terminal didn't open");
    Ok(Launched { script, files: prepared.files })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_quotes_everything_it_shows() {
        let prepared = Prepared { banner: vec!["it's isolated".into()], command: "exec true".into(), files: vec![] };
        let script = launch_script(Path::new("/Users/a/my 'app'"), "macOS sandbox", Network::LocalOnly, &prepared);
        assert!(script.contains(r"cd '/Users/a/my '\''app'\''' || exit 1"), "{script}");
        assert!(script.contains(r"echo 'it'\''s isolated'"));
        assert!(script.contains("Internet: off"));
        assert!(script.ends_with("exec true\n"));
    }

    /// The macOS sandbox, through the interface, yields a script zsh accepts.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_script_is_valid_zsh() {
        if crate::opencode::binary().is_none() {
            return;
        }
        let root = std::env::temp_dir().join(format!("superviser-backends-{}", std::process::id()));
        let project = root.join("my app");
        std::fs::create_dir_all(&project).unwrap();
        let paths = Paths::at(root.join("data"));
        let backend = backend(Isolation::MacosSandbox);
        assert!(backend.available().is_ok());
        let socket = paths.guard_socket();
        let opencode = serde_json::json!({ "permission": { "webfetch": "deny" } });
        let request = Request {
            paths: &paths,
            project: &project,
            related: &[],
            network: Network::Internet,
            guard_socket: &socket,
            model: None,
            guard_url: "http://127.0.0.1:8787",
            guard_path: "/projects/ab12",
            opencode: &opencode,
            blocked_programs: &["ssh".into()],
        };
        let prepared = backend.prepare(&request, "test").unwrap();
        assert!(prepared.files[0].exists() && prepared.command.contains("sandbox-exec"));
        let script = root.join("open.command");
        std::fs::write(&script, launch_script(&project, backend.label(), Network::Internet, &prepared)).unwrap();
        let check = std::process::Command::new("/bin/zsh").arg("-n").arg(&script).status().unwrap();
        assert!(check.success());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn every_backend_is_listed() {
        let found = choices(Path::new("/nonexistent"));
        assert_eq!(found.iter().map(|c| c.isolation).collect::<Vec<_>>(), [Isolation::MacosSandbox, Isolation::LinuxVm]);
    }
}
