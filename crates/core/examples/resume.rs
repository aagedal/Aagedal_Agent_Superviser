//! Saves a long conversation, restarts llama-server and loads it back, timing the prompt
//! before and after: what saved conversations buy, and what they take on disk.
//!
//! cargo run -p superviser-core --example resume -- <model.gguf> <work dir> [q8|f16]

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use superviser_core::catalog::KvType;
use superviser_core::conversations::{Conversations, Key};
use superviser_core::runtime::{self, LlamaServer, ServerConfig};

const CONTEXT: u64 = 32_768;

async fn start(model: &Path, work: &Path, conversations: &Conversations, kv: KvType) -> anyhow::Result<LlamaServer> {
    let key_file = work.join("key");
    std::fs::write(&key_file, "resume-example")?;
    let mut server = LlamaServer::start(ServerConfig {
        binary: runtime::find_llama_server().expect("llama-server"),
        model_path: model.to_path_buf(),
        mmproj_path: None,
        alias: "resume".into(),
        context: CONTEXT,
        port: runtime::free_port()?,
        api_key_file: key_file,
        log_file: work.join("llama-server.log"),
        prompt_cache: 2 << 30,
        decision: false,
        sampling: None,
        speculative: superviser_core::runtime::Speculative::Ngram,
        reasoning_budget: None,
        kv,
        slot_dir: Some(conversations.prepare()?.to_path_buf()),
    })
    .await?;
    server.wait_ready(Duration::from_secs(300)).await?;
    Ok(server)
}

/// Asks, and returns (tokens read, of them from cache, ms reading).
async fn ask(server: &LlamaServer, messages: &Value) -> anyhow::Result<(u64, u64, u64)> {
    let body = json!({ "messages": messages, "max_tokens": 8, "temperature": 0 });
    let reply: Value = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", server.base_url()))
        .bearer_auth("resume-example")
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let t = &reply["timings"];
    let n = |k: &str| t[k].as_f64().unwrap_or(0.0) as u64;
    Ok((n("prompt_n"), n("cache_n"), n("prompt_ms")))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let model = Path::new(&args[0]);
    let work = Path::new(&args[1]);
    let kv = if args.get(2).is_some_and(|a| a == "f16") { KvType::F16 } else { KvType::Q8 };
    std::fs::create_dir_all(work)?;
    let conversations = Conversations::new(work.join("conversations"));
    conversations.clear();
    let key = Key::of(model, kv, CONTEXT).expect("the model file");

    // A system prompt about OpenCode's size, then a short exchange.
    let filler: String = (0..1500).map(|i| format!("line {i}: the quick brown fox number {} jumps.\n", i * 7 % 101)).collect();
    let first = json!([{ "role": "system", "content": format!("You are a coding agent.\n{filler}") }, { "role": "user", "content": "Say hi." }]);
    let mut next = first.clone();
    next.as_array_mut().unwrap().extend([json!({ "role": "assistant", "content": "Hi." }), json!({ "role": "user", "content": "Say bye." })]);

    let server = start(model, work, &conversations, kv).await?;
    let (read, cached, ms) = ask(&server, &first).await?;
    println!("first ask: {read} tokens read, {cached} cached, {ms} ms");
    let saved = conversations.save(&server, "resume", &key, |tokens| tokens * 64 * 1024).await?.expect("a saved conversation");
    println!("saved {} tokens in {} MB", saved.tokens, saved.bytes / 1_000_000);
    drop(server);

    let server = start(model, work, &conversations, kv).await?;
    let restored = conversations.restore(&server, "resume", &key).await;
    println!("restored {restored:?} tokens");
    let (read, cached, ms) = ask(&server, &next).await?;
    println!("after restart: {read} tokens read, {cached} cached, {ms} ms");
    println!("on disk: {:?}", conversations.summary());
    Ok(())
}
