//! Where the app keeps its state.

use std::path::{Path, PathBuf};

use anyhow::Context;

/// Names the guard treats as the app's own control surface (self-protection): the bundle
/// identifier, which is also the state directory's name, and the app's names.
pub const PROTECTED_NAMES: &[&str] = &["me.aagedal.AgentSuperviser", "AgentSuperviser", "Agent Superviser", "agent-superviser"];

#[derive(Debug, Clone)]
pub struct Paths {
    pub data: PathBuf,
}

impl Paths {
    pub fn new() -> anyhow::Result<Self> {
        let dirs = directories::ProjectDirs::from("me", "aagedal", "AgentSuperviser")
            .context("no home directory to keep app data in")?;
        Ok(Self::at(dirs.data_dir()))
    }

    pub fn at(data: impl Into<PathBuf>) -> Self {
        Self { data: data.into() }
    }

    pub fn models(&self) -> PathBuf {
        self.data.join("models")
    }
    /// The judge's model, apart from the chat models so it is never offered as one.
    pub fn judge_model(&self) -> PathBuf {
        self.data.join("judge")
    }
    pub fn journal(&self) -> PathBuf {
        self.data.join("guard").join("calls.jsonl")
    }
    pub fn logs(&self) -> PathBuf {
        self.data.join("logs")
    }
    /// The key harnesses send to the guard. OpenCode reads it with `{file:…}`.
    pub fn local_api_key(&self) -> PathBuf {
        self.data.join("keys").join("local-api-key")
    }
    /// The key the guard sends to llama-server, which no one else is given.
    pub fn upstream_api_key(&self) -> PathBuf {
        self.data.join("keys").join("upstream-api-key")
    }
    /// The guard's Unix socket for Linux VMs. Socket paths are short (about 100 bytes), so a
    /// long home folder falls back to the per-user temporary folder.
    pub fn guard_socket(&self) -> PathBuf {
        let preferred = self.data.join("guard.sock");
        if preferred.as_os_str().len() <= 100 {
            return preferred;
        }
        let user = std::env::var("USER").unwrap_or_default();
        std::env::temp_dir().join(format!("agent-superviser-{user}.sock"))
    }

    /// Process ids of the model servers running, so a later run can stop them if this one dies.
    pub fn run(&self) -> PathBuf {
        self.data.join("run")
    }

    /// Which model each chat uses.
    pub fn chat_models(&self) -> PathBuf {
        self.data.join("chat-models.json")
    }

    pub fn settings(&self) -> PathBuf {
        self.data.join("settings.json")
    }
}

/// A program bundled with the app (Tauri puts them beside its executable, in the `.app`'s
/// `Contents/MacOS` and in `target/<profile>` when developing).
pub fn bundled(name: &str) -> Option<PathBuf> {
    let program = bundled_dir()?.join(name);
    program.is_file().then_some(program)
}

/// The folder the app's executable and its bundled programs are in.
pub fn bundled_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

/// Creates `dir` (and parents) readable by the owner only.
pub fn private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Writes `contents` to `path` readable by the owner only, replacing it atomically.
pub fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        private_dir(parent)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&tmp)?;
        std::io::Write::write_all(&mut file, contents)?;
    }
    std::fs::rename(tmp, path)
}
