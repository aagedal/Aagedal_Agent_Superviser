//! Runs llama.cpp's `llama-server` for one model, on a private port behind the guard.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use serde::Serialize;

use crate::catalog::{KvType, Sampling};

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
    /// A decision model reads its whole input in one batch: `context` tokens.
    pub decision: bool,
    /// The server's defaults; requests can still set their own.
    pub sampling: Option<Sampling>,
    /// Draft from n-grams already in the conversation. Coding agents re-emit much of what they
    /// read (whole files in edits), which this speeds up about sixfold, with the same output.
    pub speculative: bool,
    /// Thinking tokens before the model is told to answer; `None` is unlimited.
    pub reasoning_budget: Option<u32>,
    pub kv: KvType,
    /// Where it may save its conversation and load it back ([`LlamaServer::save_slot`]).
    pub slot_dir: Option<PathBuf>,
}

pub struct LlamaServer {
    child: tokio::process::Child,
    pub port: u16,
    pub alias: String,
    pub context: u64,
    api_key_file: PathBuf,
    log_file: PathBuf,
}

/// Progress reported while `llama-server` loads a model into memory.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LoadProgress {
    /// Progress fraction in `0.0..=1.0`.
    pub fraction: f32,
    /// Human-readable loading stage.
    pub stage: String,
}

/// Estimates `llama-server` loading progress from its log output.
pub fn parse_load_progress(log: &str) -> LoadProgress {
    let mut fraction: f32 = 0.04;
    let mut stage = "Starting llama-server…".to_string();

    if log.contains("llama_model_loader:") || log.contains("print_info:") {
        fraction = fraction.max(0.08);
        stage = "Reading model metadata…".into();
    }

    if let Some(idx) = log.find("load_tensors:") {
        let after = &log[idx..];
        // llama.cpp prints one `.` per 1% in its default progress callback on lines after
        // `load_tensors: loading model tensors, this can take a while...`.
        let mut dots: usize = 0;
        let mut saw_header = false;
        for line in after.lines() {
            let trimmed = line.trim();
            if !saw_header {
                if trimmed.contains("loading model tensors") {
                    saw_header = true;
                }
                continue;
            }
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.bytes().all(|b| b == b'.') {
                dots += trimmed.len();
            } else if trimmed.starts_with('.') {
                let run = trimmed.bytes().take_while(|&b| b == b'.').count();
                dots += run;
                break;
            } else {
                break;
            }
        }
        let clamped_dots = dots.min(100);
        let weight_frac = 0.12 + (clamped_dots as f32 / 100.0) * 0.66;
        if weight_frac > fraction {
            fraction = weight_frac;
        }
        stage = if clamped_dots > 0 && clamped_dots < 100 {
            format!("Loading weights ({clamped_dots}%)…")
        } else {
            "Loading weights…".into()
        };

        if after.contains("offloaded ") || after.contains("model buffer size") {
            fraction = fraction.max(0.78);
            stage = "Loading weights…".into();
        }
    }

    if log.contains("llama_context:") || log.contains("llama_kv_cache") || log.contains("sched_reserve:") || log.contains("graph_reserve:") {
        fraction = fraction.max(0.86);
        stage = "Allocating context & KV cache…".into();
    }

    if log.contains("warmup:") {
        fraction = fraction.max(0.92);
        stage = "Warming up…".into();
    }

    if log.contains("server is listening") || log.contains("starting the main loop") {
        fraction = fraction.max(0.95);
        stage = "Finishing startup…".into();
    }

    LoadProgress {
        fraction: fraction.clamp(0.0, 0.99),
        stage,
    }
}

/// Its conversation as `/slots` describes it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Slot {
    pub tokens: u64,
    pub busy: bool,
}

/// A conversation written to the slot folder.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Saved {
    pub tokens: u64,
    pub bytes: u64,
}

/// Saving waits for a reply in progress; restoring reads a few GB at most.
const SLOT_TIMEOUT: Duration = Duration::from_secs(60);

/// The folder the app bundles llama.cpp's build in, with its libraries.
const BUNDLED_FOLDER: &str = "llama.cpp";

/// [`BINARY_ENV`], then the build bundled with the app, then `PATH`.
pub fn find_llama_server() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(BINARY_ENV) {
        return Some(PathBuf::from(path));
    }
    let name = if cfg!(windows) { "llama-server.exe" } else { "llama-server" };
    crate::paths::bundled_in(BUNDLED_FOLDER, name).or_else(|| crate::paths::bundled(name)).or_else(|| which::which("llama-server").ok())
}

/// What a model reads when its reasoning budget runs out, before thinking ends: a nudge to act
/// rather than an abrupt cut mid-sentence.
const BUDGET_MESSAGE: &str = "\n\nI've thought about this long enough, so I'll act on what I have now.\n";

/// A port nothing listens on right now.
pub fn free_port() -> std::io::Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

pub fn args(config: &ServerConfig) -> Vec<String> {
    let mut args = vec![
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
    ];
    if config.kv != KvType::F16 {
        for flag in ["--cache-type-k", "--cache-type-v"] {
            args.extend([flag.to_string(), config.kv.llama_name().to_string()]);
        }
    }
    if let Some(dir) = &config.slot_dir {
        args.extend(["--slot-save-path".into(), dir.display().to_string()]);
    }
    if config.decision {
        for flag in ["--batch-size", "--ubatch-size"] {
            args.extend([flag.to_string(), config.context.to_string()]);
        }
    }
    if config.speculative {
        args.extend(["--spec-type".into(), "ngram-mod".into()]);
    }
    if let Some(budget) = config.reasoning_budget {
        args.extend(["--reasoning-budget".into(), budget.to_string(), "--reasoning-budget-message".into(), BUDGET_MESSAGE.into()]);
    }
    if let Some(s) = config.sampling {
        let flags = [
            ("--temp", s.temperature.to_string()),
            ("--top-p", s.top_p.to_string()),
            ("--top-k", s.top_k.to_string()),
            ("--min-p", s.min_p.to_string()),
            ("--presence-penalty", s.presence_penalty.to_string()),
            ("--repeat-penalty", s.repeat_penalty.to_string()),
        ];
        args.extend(flags.into_iter().flat_map(|(flag, value)| [flag.to_string(), value]));
    }
    args
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
        Ok(Self {
            child,
            port: config.port,
            alias: config.alias,
            context: config.context,
            api_key_file: config.api_key_file,
            log_file: config.log_file,
        })
    }

    fn authorized(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match std::fs::read_to_string(&self.api_key_file) {
            Ok(key) => request.bearer_auth(key.trim()),
            Err(_) => request,
        }
    }

    /// The conversation in its one slot.
    pub async fn slot(&self) -> anyhow::Result<Slot> {
        let request = reqwest::Client::new().get(format!("{}/slots", self.base_url())).timeout(Duration::from_secs(5));
        let slots: serde_json::Value = self.authorized(request).send().await?.error_for_status()?.json().await?;
        let slot = slots.get(0).context("llama-server listed no slots")?;
        Ok(Slot { tokens: slot["n_prompt_tokens"].as_u64().unwrap_or(0), busy: slot["is_processing"].as_bool().unwrap_or(false) })
    }

    async fn slot_action(&self, action: &str, filename: &str) -> anyhow::Result<serde_json::Value> {
        let url = format!("{}/slots/0?action={action}", self.base_url());
        let request = reqwest::Client::new().post(url).json(&serde_json::json!({ "filename": filename })).timeout(SLOT_TIMEOUT);
        let response = self.authorized(request).send().await?;
        let status = response.status();
        let body: serde_json::Value = response.json().await.unwrap_or_default();
        if !status.is_success() {
            bail!("llama-server couldn't {action} the conversation ({status}): {}", body["error"]["message"].as_str().unwrap_or(""));
        }
        Ok(body)
    }

    /// Writes its conversation to `filename` in the slot folder.
    pub async fn save_slot(&self, filename: &str) -> anyhow::Result<Saved> {
        let body = self.slot_action("save", filename).await?;
        Ok(Saved { tokens: body["n_saved"].as_u64().unwrap_or(0), bytes: body["n_written"].as_u64().unwrap_or(0) })
    }

    /// Loads a conversation saved by [`LlamaServer::save_slot`]; returns its tokens.
    pub async fn restore_slot(&self, filename: &str) -> anyhow::Result<u64> {
        let body = self.slot_action("restore", filename).await?;
        Ok(body["n_restored"].as_u64().unwrap_or(0))
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Waits until the model is loaded (`/health` answers 200) or the process exits.
    pub async fn wait_ready(&mut self, timeout: Duration) -> anyhow::Result<()> {
        self.wait_ready_with_progress(timeout, |_| {}).await
    }

    /// Like [`LlamaServer::wait_ready`], calling `on_progress` as loading advances.
    pub async fn wait_ready_with_progress(
        &mut self,
        timeout: Duration,
        mut on_progress: impl FnMut(LoadProgress),
    ) -> anyhow::Result<()> {
        let client = reqwest::Client::new();
        let url = format!("{}/health", self.base_url());
        let start = Instant::now();
        let mut last_frac: f32 = -1.0;
        let mut last_stage = String::new();
        loop {
            if let Some(status) = self.child.try_wait()? {
                bail!("llama-server exited while loading ({status})");
            }
            if let Ok(response) = self.authorized(client.get(&url).timeout(Duration::from_secs(2))).send().await
                && response.status().is_success()
                && response.json::<serde_json::Value>().await.is_ok_and(|v| v["status"] == "ok")
            {
                return Ok(());
            }
            if let Ok(log) = std::fs::read_to_string(&self.log_file) {
                let mut progress = parse_load_progress(&log);
                if progress.fraction < last_frac {
                    progress.fraction = last_frac;
                }
                if (progress.fraction - last_frac) >= 0.01 || progress.stage != last_stage {
                    last_frac = progress.fraction;
                    last_stage = progress.stage.clone();
                    on_progress(progress);
                }
            }
            if start.elapsed() > timeout {
                bail!("llama-server did not load the model within {}s", timeout.as_secs());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Its process id, while it runs.
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub async fn stop(&mut self) {
        let _ = self.child.kill().await;
    }
}

/// Stops llama-servers a previous run of the app left behind (it was killed, or crashed),
/// listed as `<name>.pid` files in `dir`, and removes the files. A pid is only stopped if it
/// still runs llama-server with `marker` (the app's key file) in its arguments, so a reused pid
/// or the user's own llama-server is left alone.
pub fn stop_leftovers(dir: &Path, marker: &str) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "pid") {
            continue;
        }
        let matches_server = |pid: u32| {
            std::process::Command::new("/bin/ps")
                .args(["-o", "command=", "-p", &pid.to_string()])
                .output()
                .is_ok_and(|out| {
                    let command = String::from_utf8_lossy(&out.stdout);
                    command.contains("llama-server") && command.contains(marker)
                })
        };
        if let Some(pid) = std::fs::read_to_string(&path).ok().and_then(|t| t.trim().parse::<u32>().ok())
            && matches_server(pid)
        {
            tracing::info!("stopping a model server left from an earlier run (pid {pid})");
            let _ = std::process::Command::new("/bin/kill").arg(pid.to_string()).status();
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline && matches_server(pid) {
                std::thread::sleep(Duration::from_millis(100));
            }
            if matches_server(pid) {
                tracing::warn!("force-killing unresponsive leftover llama-server (pid {pid})");
                let _ = std::process::Command::new("/bin/kill").args(["-KILL", &pid.to_string()]).status();
            }
        }
        let _ = std::fs::remove_file(path);
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
            decision: false,
            sampling: None,
            speculative: false,
            reasoning_budget: None,
            kv: KvType::F16,
            slot_dir: None,
        };
        assert!(!args(&config).iter().any(|a| a == "--ubatch-size"));
        assert!(!args(&config).iter().any(|a| a == "--cache-type-k" || a == "--slot-save-path"));
        let q8 = args(&ServerConfig { kv: KvType::Q8, slot_dir: Some("/c/slots".into()), ..config.clone() });
        for pair in [["--cache-type-k", "q8_0"], ["--cache-type-v", "q8_0"], ["--slot-save-path", "/c/slots"]] {
            assert!(q8.windows(2).any(|w| w == pair), "{pair:?} in {q8:?}");
        }
        let decision = args(&ServerConfig { decision: true, ..config.clone() });
        assert!(decision.windows(2).any(|w| w == ["--ubatch-size", "32768"]));
        let args = args(&config);
        assert!(args.windows(2).any(|w| w == ["--api-key-file", "/k/key"]));
        assert!(!args.iter().any(|a| a == "--api-key"));
        assert!(args.windows(2).any(|w| w == ["--host", "127.0.0.1"]));
        assert!(args.windows(2).any(|w| w == ["--cache-ram", "1536"]));
        assert!(!args.iter().any(|a| a == "--temp" || a == "--spec-type" || a == "--reasoning-budget"));
    }

    #[test]
    fn sampling_becomes_server_defaults() {
        let sampling = Sampling { temperature: 0.6, top_p: 0.95, top_k: 20, min_p: 0.0, presence_penalty: 0.0, repeat_penalty: 1.0 };
        let config = ServerConfig {
            binary: "llama-server".into(),
            model_path: "/m/a.gguf".into(),
            alias: "a".into(),
            context: 32768,
            port: 1234,
            api_key_file: "/k/key".into(),
            log_file: "/l/log".into(),
            prompt_cache: 0,
            decision: false,
            sampling: Some(sampling),
            speculative: true,
            reasoning_budget: Some(16384),
            kv: KvType::Q8,
            slot_dir: None,
        };
        let args = args(&config);
        for pair in [["--spec-type", "ngram-mod"], ["--reasoning-budget", "16384"], ["--temp", "0.6"], ["--top-p", "0.95"], ["--top-k", "20"], ["--min-p", "0"], ["--repeat-penalty", "1"]] {
            assert!(args.windows(2).any(|w| w == pair), "{pair:?} in {args:?}");
        }
    }

    #[test]
    fn parses_llama_server_loading_stages_and_dots() {
        let p0 = parse_load_progress("");
        assert!(p0.fraction < 0.1);

        let p1 = parse_load_progress("llama_model_loader: loaded meta data\n");
        assert!(p1.fraction > p0.fraction);

        let log_half = "llama_model_loader: ok\nload_tensors: loading model tensors, this can take a while... (mmap = true)\n..................................................\n";
        let p2 = parse_load_progress(log_half);
        assert!((p2.fraction - 0.45).abs() < 0.05, "got {}", p2.fraction);
        assert!(p2.stage.contains("50%"));

        let log_ctx = format!("{log_half}load_tensors: offloaded 32/32 layers to GPU\nllama_context: constructing llama_context\n");
        let p3 = parse_load_progress(&log_ctx);
        assert!(p3.fraction >= 0.85);

        let log_listen = format!("{log_ctx}warmup: warming up\nmain: server is listening on http://127.0.0.1:8080\n");
        let p4 = parse_load_progress(&log_listen);
        assert!(p4.fraction >= 0.95);
    }
}
