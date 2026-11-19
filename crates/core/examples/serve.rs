//! Runs an installed model behind the guard until killed, printing every reviewed call.
//!
//! cargo run -p superviser-core --example serve -- <data dir> <port> <client key file> [unix socket]

use std::sync::Arc;
use std::time::Duration;

use superviser_core::paths::Paths;
use superviser_core::runtime::{self, LlamaServer, ServerConfig};
use superviser_core::{installed, secrets};
use superviser_guard::{CallEvent, Config, GuardServer, ProxyOptions};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let paths = Paths::at(&args[0]);
    let port: u16 = args[1].parse()?;
    let client_key = std::fs::read_to_string(&args[2])?.trim().to_string();
    let model = installed::list(&paths).into_iter().next().expect("an installed model");
    let mut server = LlamaServer::start(ServerConfig {
        binary: runtime::find_llama_server().expect("llama-server"),
        model_path: model.entry_path().unwrap(),
        alias: model.alias.clone(),
        context: 32768,
        port: runtime::free_port()?,
        api_key_file: paths.upstream_api_key(),
        log_file: paths.logs().join("llama-server.log"),
        prompt_cache: 2 << 30,
        decision: false,
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
