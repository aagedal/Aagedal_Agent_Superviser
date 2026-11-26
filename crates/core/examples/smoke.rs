//! End-to-end check: download a model into a scratch directory, run llama-server, put the
//! guard in front, and send a chat completion (whole and streamed) through it.
//!
//! cargo run -p superviser-core --example smoke -- unsloth/Qwen3.5-0.8B-GGUF /tmp/superviser-smoke

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use superviser_core::catalog::{Catalog, CatalogModel};
use superviser_core::hub::Hub;
use superviser_core::paths::Paths;
use superviser_core::runtime::{self, LlamaServer, ServerConfig};
use superviser_core::{installed, secrets};
use superviser_guard::{Config, GuardServer, ProxyOptions};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let repo = args.next().unwrap_or_else(|| "unsloth/Qwen3.5-0.8B-GGUF".into());
    let dir = args.next().expect("scratch directory");
    let paths = Paths::at(&dir);
    let hub = Hub::new(None);

    let model = CatalogModel::resolve(&hub, &repo, None, "").await?;
    println!("resolved {} {} ({} bytes, kv {:?} B/token) @ {}", model.repo, model.quant.label, model.quant.size, model.kv_bytes_per_token, &model.revision[..8]);
    let mut last = 0;
    let installed = installed::install(&hub, &paths, &model, Arc::new(AtomicBool::new(false)), |p| {
        let pct = p.done * 100 / p.total.max(1);
        if pct >= last + 25 {
            last = pct;
            println!("  {pct}%");
        }
    })
    .await?;
    println!("installed and verified: {}", installed.entry_path().unwrap().display());
    assert_eq!(installed::list(&paths).len(), 1);

    let upstream_key = secrets::upstream_api_key(&paths)?;
    let catalog = Catalog::bundled();
    let mut server = LlamaServer::start(ServerConfig {
        binary: runtime::find_llama_server().expect("llama-server on PATH"),
        model_path: installed.entry_path().unwrap(),
        alias: installed.alias.clone(),
        context: 8192,
        port: runtime::free_port()?,
        api_key_file: paths.upstream_api_key(),
        log_file: paths.logs().join("llama-server.log"),
        prompt_cache: 0,
        decision: false,
        sampling: catalog.curated(&installed.model.repo).and_then(|m| m.sampling),
        reasoning_budget: catalog.curated(&installed.model.repo).and_then(|m| m.reasoning_budget),
        kv: superviser_core::catalog::KvType::Q8,
        slot_dir: None,
        speculative: superviser_core::runtime::Speculative::Ngram,
    })
    .await?;
    server.wait_ready(Duration::from_secs(120)).await?;
    println!("llama-server ready at {}", server.base_url());

    let client = reqwest::Client::new();
    let direct = client.get(format!("{}/v1/models", server.base_url())).send().await?;
    println!("llama-server without its key: {}", direct.status());
    assert_eq!(direct.status(), 401);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let options = ProxyOptions { api_key: Some("client".into()), upstream_api_key: Some(upstream_key), cors_origins: vec![], judge: None, on_model_call: None };
    let guard = GuardServer::start(listener, Config::default(), options).await?;
    guard.set_upstream(Some(server.base_url()));
    let url = format!("http://{}/v1/chat/completions", guard.addr);

    let body = serde_json::json!({
        "model": installed.alias,
        "messages": [{"role": "user", "content": "Reply with the single word: hello"}],
        "max_tokens": 64,
    });
    let whole: serde_json::Value = client.post(&url).bearer_auth("client").json(&body).send().await?.error_for_status()?.json().await?;
    println!("whole reply via guard: {:?}", whole["choices"][0]["message"]["content"]);

    let mut stream_body = body.clone();
    stream_body["stream"] = true.into();
    let text = client.post(&url).bearer_auth("client").json(&stream_body).send().await?.error_for_status()?.text().await?;
    println!("streamed via guard: {} events, ends with [DONE]: {}", text.matches("data: ").count(), text.trim_end().ends_with("data: [DONE]"));

    server.stop().await;
    Ok(())
}
