//! Where the app keeps its state.

use std::path::{Path, PathBuf};

use anyhow::Context;

/// Names the guard treats as the app's own control surface (self-protection): the bundle
/// identifier, which is also the state directory's name, and the app's names.
pub const PROTECTED_NAMES: &[&str] = &["me.aagedal.AgentSuperviser", "AgentSuperviser", "Agent Superviser", "agent-superviser"];

#[derive(Debug, Clone)]
pub struct Paths {
    pub data: PathBuf,
    /// What can be deleted at any time: `~/Library/Caches/<bundle id>` on macOS.
    pub cache: PathBuf,
}

impl Paths {
    pub fn new() -> anyhow::Result<Self> {
        let dirs = directories::ProjectDirs::from("me", "aagedal", "AgentSuperviser")
            .context("no home directory to keep app data in")?;
        Ok(Self { data: dirs.data_dir().to_path_buf(), cache: dirs.cache_dir().to_path_buf() })
    }

    /// Everything under `data`, the cache too.
    pub fn at(data: impl Into<PathBuf>) -> Self {
        let data = data.into();
        Self { cache: data.join("cache"), data }
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
        let user: String = std::env::var("USER")
            .unwrap_or_default()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            .take(32)
            .collect();
        let user = if user.is_empty() || user == "." || user == ".." { "user" } else { &user };
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

    /// Chats put away out of the chat list.
    pub fn archived_chats(&self) -> PathBuf {
        self.data.join("archived-chats.json")
    }

    /// Each model's last conversation, saved by llama-server so it needn't be read again.
    pub fn saved_conversations(&self) -> PathBuf {
        self.cache.join("conversations")
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

/// A program bundled with the app in a folder of its own (with the libraries it needs): in the
/// `.app`'s `Contents/Resources/<folder>`, or beside the executable when developing.
pub fn bundled_in(folder: &str, name: &str) -> Option<PathBuf> {
    let dir = bundled_dir()?;
    [dir.join("../Resources").join(folder).join(name), dir.join(folder).join(name)].into_iter().find(|p| p.is_file())
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
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        private_dir(parent)?;
    }
    let mut nonce = [0u8; 8];
    getrandom::fill(&mut nonce).map_err(|e| std::io::Error::other(e.to_string()))?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = path.with_file_name(format!(".{name}.{}.{}.tmp", std::process::id(), hex::encode(nonce)));
    let write_result = (|| -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&tmp)?;
        std::io::Write::write_all(&mut file, contents)?;
        std::io::Write::flush(&mut file)?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}
