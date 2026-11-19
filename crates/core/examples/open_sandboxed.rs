//! Opens a folder as a project in isolated OpenCode, as the app does:
//! `cargo run -p superviser-core --example open_sandboxed -- PROJECT [local] [linux-vm]`.
//! OpenCode talks to the guard at `/projects/<id>/v1`, which only the app's guard serves.

use superviser_core::paths::Paths;
use superviser_core::project::Project;
use superviser_core::sandbox::{Isolation, Launch, Network, choices, launch};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let folder = std::path::Path::new(&args[0]);
    for choice in choices(folder) {
        println!("{:<14} {}", choice.label, choice.unavailable.as_deref().unwrap_or("available"));
    }
    let mut project = Project::new(folder.to_path_buf());
    project.network = if args.iter().any(|a| a == "local") { Network::LocalOnly } else { Network::Internet };
    project.isolation = if args.iter().any(|a| a == "linux-vm") { Isolation::LinuxVm } else { Isolation::MacosSandbox };
    // The macOS sandbox needs no model; the Linux VM writes OpenCode's config and needs one.
    let request = Launch { project: &project, model: None, guard_url: "http://127.0.0.1:8787".into(), opencode: serde_json::json!({}) };
    let launched = launch(&Paths::new()?, &request)?;
    println!("script {}", launched.script.display());
    Ok(())
}
