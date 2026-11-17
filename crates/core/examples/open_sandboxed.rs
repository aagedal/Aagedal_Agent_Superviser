//! Opens a project in sandboxed OpenCode, as the app's menu does:
//! `cargo run -p superviser-core --example open_sandboxed -- PROJECT [local]`.

use superviser_core::paths::Paths;
use superviser_core::sandbox::{Network, launch_opencode};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let network = if args.get(1).is_some_and(|a| a == "local") { Network::LocalOnly } else { Network::Internet };
    let launched = launch_opencode(&Paths::new()?, args[0].as_ref(), network)?;
    println!("profile {}\nscript  {}", launched.profile.display(), launched.script.display());
    Ok(())
}
