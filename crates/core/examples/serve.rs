//! Runs an installed model behind the guard until killed, printing every reviewed call.
//!
//! cargo run -p superviser-core --example serve -- <data dir> <port> <client key file> [unix socket]

use std::sync::Arc;
use std::time::Duration;

use superviser_core::catalog::Catalog;
use superviser_core::paths::Paths;
use superviser_core::runtime::{self, DEFAULT_SPEC_DRAFT_N_MAX, LlamaServer, ServerConfig};
use superviser_core::{installed, secrets};
use superviser_guard::{CallEvent, Config, GuardServer, ProxyOptions};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let paths = Paths::at(&args[0]);
    let port: u16 = args[1].parse()?;
    let client_key = std::fs::read_to_string(&args[2])?.trim().to_string();
    let model = installed::list(&paths).into_iter().next().expect("an installed model");
    let catalog = Catalog::bundled();
    let entry = model.entry_path().unwrap();
    let (speculative, _) = runtime::detect_speculative(&entry, u64::MAX, DEFAULT_SPEC_DRAFT_N_MAX);
    let mmproj_path = runtime::find_companion_mmproj(&entry);
    let mut server = LlamaServer::start(ServerConfig {
        binary: runtime::find_llama_server().expect("llama-server"),
        model_path: entry,
        mmproj_path,
        alias: model.alias.clone(),
        context: 32768,
        port: runtime::free_port()?,
        api_key_file: paths.upstream_api_key(),
        log_file: paths.logs().join("llama-server.log"),
        prompt_cache: 2 << 30,
        decision: false,
        sampling: catalog.curated(&model.model.repo).and_then(|m| m.sampling),
        reasoning_budget: catalog.curated(&model.model.repo).and_then(|m| m.reasoning_budget),
        kv: superviser_core::catalog::KvType::Q8,
        slot_dir: None,
        speculative,
    })
    .await?;
    server.wait_ready(Duration::from_secs(120)).await?;
    let config = Config {
        on_event: Some(Arc::new(|e: &CallEvent| println!("guard: {:?} {} {} {:?} {:?}", e.verdict, e.tool, e.summary, e.reason, e.judge))),
        ..Config::default()
    };
    let options = ProxyOptions {
        api_key: Some(client_key),
        upstream_api_key: Some(secrets::upstream_api_key(&paths)?),
        cors_origins: vec![],
        judge: None,
        on_model_call: None,
    };
    let guard = GuardServer::start(tokio::net::TcpListener::bind(("127.0.0.1", port)).await?, config, options).await?;
    guard.set_upstream(Some(server.base_url()));
    println!("ready: {} as {}", guard.addr, model.alias);
    // An optional Unix socket to serve too (what Linux VMs reach).
    if let Some(socket) = args.get(3) {
        guard.listen_unix(socket.as_ref())?;
        println!("also serving {socket}");
    }
    tokio::signal::ctrl_c().await?;
    server.stop().await;
    Ok(())
}
