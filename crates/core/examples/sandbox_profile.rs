//! Writes the sandbox profile and launch script for a project without launching anything:
//! `cargo run -p superviser-core --example sandbox_profile -- PROJECT OUT_DIR [local]`.

use superviser_core::paths::Paths;
use superviser_core::sandbox::{Network, Policy, launch_script};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let network = if args.get(2).is_some_and(|a| a == "local") { Network::LocalOnly } else { Network::Internet };
    let policy = Policy::new(args[0].as_ref(), &Paths::new()?, network)?;
    let out = std::path::Path::new(&args[1]);
    std::fs::write(out.join("app.sb"), policy.profile())?;
    std::fs::write(out.join("open.command"), launch_script(&policy, &out.join("app.sb"), &which::which("opencode")?))?;
    Ok(())
}
