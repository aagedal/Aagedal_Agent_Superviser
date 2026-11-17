//! Prepares (without launching) a Linux VM launch for a project and prints its script:
//! `cargo run -p superviser-core --example vm_script -- PROJECT DATA_DIR [local]`.

use superviser_core::opencode::SetupModel;
use superviser_core::paths::Paths;
use superviser_core::sandbox::{Backend, Network, Request, container::LinuxVm, launch_script};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let project = std::path::Path::new(&args[0]);
    let paths = Paths::at(&args[1]);
    let network = if args.iter().any(|a| a == "local") { Network::LocalOnly } else { Network::Internet };
    let model = SetupModel { alias: "qwen3.5-0.8b".into(), name: "Qwen3.5 0.8B".into(), context: 32768 };
    let socket = paths.guard_socket();
    let prepared = LinuxVm.prepare(&Request { paths: &paths, project, network, guard_socket: &socket, model: Some(model) }, "smoke")?;
    eprintln!("guard socket {}", socket.display());
    print!("{}", launch_script(project, "Linux VM", network, &prepared));
    Ok(())
}
