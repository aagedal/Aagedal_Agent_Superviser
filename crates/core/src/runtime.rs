//! Runs llama.cpp's `llama-server` for one model, on a private port behind the guard.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};

/// Overrides where llama-server is found.
pub const BINARY_ENV: &str = "SUPERVISER_LLAMA_SERVER";

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub binary: PathBuf,
    pub model_path: PathBuf,
    /// The model name the API answers to.
    pub alias: String,
    pub context: u64,
    pub port: u16,
    pub api_key_file: PathBuf,
    pub log_file: PathBuf,
    /// Bytes of RAM for saved conversations; llama-server's own default is 8 GiB.
    pub prompt_cache: u64,
}

pub struct LlamaServer {
    child: tokio::process::Child,
    pub port: u16,
    pub alias: String,
    pub context: u64,
}

/// The bundled sidecar next to the app's executable, then `PATH`.
pub fn find_llama_server() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(BINARY_ENV) {
        return Some(PathBuf::from(path));
    }
    let name = if cfg!(windows) { "llama-server.exe" } else { "llama-server" };
    if let Some(dir) = std::env::current_exe().ok().and_then(|exe| exe.parent().map(Path::to_path_buf)) {
        let bundled = dir.join(name);
        if bundled.is_file() {
            return Some(bundled);
        }
    }
    which::which("llama-server").ok()
}

/// A port nothing listens on right now.
pub fn free_port() -> std::io::Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

pub fn args(config: &ServerConfig) -> Vec<String> {
    vec![
        "--model".into(),
        config.model_path.display().to_string(),
        "--alias".into(),
        config.alias.clone(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        config.port.to_string(),
        "--ctx-size".into(),
        config.context.to_string(),
        // One slot gets the whole context we sized memory for.
        "--parallel".into(),
        "1".into(),
        "--cache-ram".into(),
        (config.prompt_cache >> 20).to_string(),
        "--jinja".into(),
        "--no-webui".into(),
        "--api-key-file".into(),
        config.api_key_file.display().to_string(),
    ]
}

impl LlamaServer {
    pub async fn start(config: ServerConfig) -> anyhow::Result<Self> {
        if let Some(parent) = config.log_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let log = std::fs::File::create(&config.log_file)?;
        let child = tokio::process::Command::new(&config.binary)
            .args(args(&config))
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting {}", config.binary.display()))?;
        Ok(Self { child, port: config.port, alias: config.alias, context: config.context })
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Waits until the model is loaded (`/health` answers 200) or the process exits.
    pub async fn wait_ready(&mut self, timeout: Duration) -> anyhow::Result<()> {
        let client = reqwest::Client::new();
        let url = format!("{}/health", self.base_url());
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait()? {
                bail!("llama-server exited while loading ({status})");
            }
            if let Ok(response) = client.get(&url).timeout(Duration::from_secs(2)).send().await
                && response.status().is_success()
            {
                return Ok(());
            }
            if start.elapsed() > timeout {
                bail!("llama-server did not load the model within {}s", timeout.as_secs());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub async fn stop(&mut self) {
        let _ = self.child.kill().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_passed_by_file_not_argument() {
        let config = ServerConfig {
            binary: "llama-server".into(),
            model_path: "/m/a.gguf".into(),
            alias: "a".into(),
            context: 32768,
            port: 1234,
            api_key_file: "/k/key".into(),
            log_file: "/l/log".into(),
            prompt_cache: 1536 << 20,
        };
        let args = args(&config);
        assert!(args.windows(2).any(|w| w == ["--api-key-file", "/k/key"]));
        assert!(!args.iter().any(|a| a == "--api-key"));
        assert!(args.windows(2).any(|w| w == ["--host", "127.0.0.1"]));
        assert!(args.windows(2).any(|w| w == ["--cache-ram", "1536"]));
    }
}
