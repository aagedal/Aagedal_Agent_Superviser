//! Opens a project in isolated OpenCode, as the app's menu does:
//! `cargo run -p superviser-core --example open_sandboxed -- PROJECT [local] [linux-vm]`.

use superviser_core::paths::Paths;
use superviser_core::sandbox::{Isolation, Network, choices, launch};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let project = std::path::Path::new(&args[0]);
    for choice in choices(project) {
        println!("{:<14} {}", choice.label, choice.unavailable.as_deref().unwrap_or("available"));
    }
    let network = if args.iter().any(|a| a == "local") { Network::LocalOnly } else { Network::Internet };
    let isolation = if args.iter().any(|a| a == "linux-vm") { Isolation::LinuxVm } else { Isolation::MacosSandbox };
    // The macOS sandbox needs no model; the Linux VM writes OpenCode's config and needs one.
    let launched = launch(&Paths::new()?, project, isolation, network, None)?;
    println!("script {}", launched.script.display());
    Ok(())
}
