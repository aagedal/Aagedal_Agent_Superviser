//! Prepares (without launching) a Linux VM launch for a project and prints its script:
//! `cargo run -p superviser-core --example vm_script -- PROJECT DATA_DIR [local] [RELATED…]`.
//! Related folders are shared read-only.

use superviser_core::opencode::SetupModel;
use superviser_core::paths::Paths;
use superviser_core::project::Folder;
use superviser_core::sandbox::{Backend, Network, Request, container::LinuxVm, launch_script};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let project = std::path::Path::new(&args[0]);
    let paths = Paths::at(&args[1]);
    let network = if args.iter().any(|a| a == "local") { Network::LocalOnly } else { Network::Internet };
    let related: Vec<Folder> = args[2..].iter().filter(|a| *a != "local").map(|p| Folder { path: p.into(), writable: false }).collect();
    let model = SetupModel { alias: "qwen3.5-0.8b".into(), name: "Qwen3.5 0.8B".into(), context: 32768 };
    let socket = paths.guard_socket();
    let opencode = serde_json::json!({ "permission": { "webfetch": "deny" } });
    let request = Request {
        paths: &paths,
        project,
        related: &related,
        network,
        guard_socket: &socket,
        model: Some(model),
        guard_url: "http://127.0.0.1:8787",
        guard_path: "/projects/smoke",
        opencode: &opencode,
        blocked_programs: &[],
    };
    let prepared = LinuxVm.prepare(&request, "smoke")?;
    eprintln!("guard socket {}", socket.display());
    print!("{}", launch_script(project, "Linux VM", network, &prepared));
    Ok(())
}
