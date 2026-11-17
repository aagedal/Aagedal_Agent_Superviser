//! The Linux VM backend: OpenCode in a Linux container on Apple's `container` (one
//! lightweight VM per container, on the Virtualization framework), sharing only the
//! project folder. Optional: projects that need macOS tools stay on the macOS sandbox.
//!
//! The image (`Containerfile`: Debian, common build tools and OpenCode at the host's
//! version) is built once per OpenCode version, in the Terminal window, on first launch.
//! The VM sees the project as `/workspace`, OpenCode's config (read-only) and its own
//! session state; nothing else of the Mac. OpenCode reaches models through the guard's
//! Unix socket, which `container` forwards into the VM with its SSH-agent forwarding
//! (`--ssh` forwards whatever `SSH_AUTH_SOCK` names; the user's own agent is never
//! forwarded). A relay in the VM turns it back into `127.0.0.1:8787`. Nothing on the Mac
//! listens on the network for it, so the firewall never asks. With internet off, the VM is
//! on a host-only network.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use serde::Deserialize;

use crate::hardware::Hardware;
use crate::paths::write_private;
use crate::{opencode, secrets};

use super::{Backend, Isolation, Network, Prepared, Request, Setup, shell_quote};

const CONTAINERFILE: &str = include_str!("Containerfile");
/// Bumped whenever `Containerfile` changes, so images are rebuilt.
const RECIPE: u32 = 3;
const IMAGE: &str = "agent-superviser/opencode";
/// The OpenCode version used when the host's can't be read.
const DEFAULT_OPENCODE: &str = "2.3.0";
/// The host-only network used when internet is off.
const LOCAL_NETWORK: &str = "superviser-local";
const HOME: &str = "/home/agent";
/// Where the relay in the VM serves the guard.
const GUARD_IN_VM: &str = "http://127.0.0.1:8787/v1";

/// The first macOS major version Apple's `container` supports.
const MIN_MACOS: u32 = 26;
/// Folders not worth searching for project markers.
const SKIPPED: &[&str] = &[".git", "node_modules", "target", "build", "dist", ".build", "Pods", "DerivedData", ".venv"];
const APPLE_PLATFORMS: &[&str] = &[".iOS(", ".macOS(", ".tvOS(", ".watchOS(", ".visionOS(", ".macCatalyst("];

/// Apple's releases of `container`.
pub const RELEASES: &str = "https://github.com/apple/container/releases/latest";
const LATEST_API: &str = "https://api.github.com/repos/apple/container/releases/latest";
/// The installer Apple signs and notarizes (not the unsigned one or debug symbols).
const SIGNED_INSTALLER: &str = "-installer-signed.pkg";

#[derive(Deserialize)]
struct Release {
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

/// The download link of the latest signed `container` installer, else the release page.
pub async fn installer_url() -> String {
    let found = async {
        let release: Release = reqwest::Client::builder()
            .user_agent(concat!("agent-superviser/", env!("CARGO_PKG_VERSION")))
            .build()
            .ok()?
            .get(LATEST_API)
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        release.assets.into_iter().find(|a| a.name.ends_with(SIGNED_INSTALLER)).map(|a| a.browser_download_url)
    }
    .await;
    found.filter(|url| url.starts_with("https://github.com/apple/container/releases/download/")).unwrap_or_else(|| RELEASES.to_string())
}

/// Whether `container` is missing (rather than the Mac being too old for it).
fn tool_missing() -> bool {
    cfg!(all(target_os = "macos", target_arch = "aarch64")) && macos_major().is_some_and(|v| v >= MIN_MACOS) && which::which("container").is_err()
}

pub struct LinuxVm;

fn macos_major() -> Option<u32> {
    let out = std::process::Command::new("/usr/bin/sw_vers").arg("-productVersion").output().ok()?;
    String::from_utf8_lossy(&out.stdout).trim().split('.').next()?.parse().ok()
}

/// Why a project needs macOS's own tools, if it does: an Xcode project or workspace,
/// CocoaPods, or a Swift package for Apple platforms (within two levels of the root).
pub fn needs_macos_tools(project: &Path) -> Option<String> {
    let mut stack = vec![(project.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".xcodeproj") || name.ends_with(".xcworkspace") {
                return Some(format!("it has an Xcode project ({name})"));
            }
            if name == "Podfile" {
                return Some("it uses CocoaPods".into());
            }
            if name == "Package.swift"
                && let Ok(manifest) = std::fs::read_to_string(&path)
                && APPLE_PLATFORMS.iter().any(|p| manifest.contains(p))
            {
                return Some("it's a Swift package for Apple platforms".into());
            }
            if path.is_dir() && depth < 2 && !SKIPPED.contains(&name.as_str()) {
                stack.push((path, depth + 1));
            }
        }
    }
    None
}

impl Backend for LinuxVm {
    fn isolation(&self) -> Isolation {
        Isolation::LinuxVm
    }

    fn label(&self) -> &'static str {
        "Linux VM"
    }

    fn available(&self) -> Result<(), String> {
        if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            return Err("needs a Mac with Apple silicon".into());
        }
        if macos_major().is_none_or(|v| v < MIN_MACOS) {
            return Err(format!("needs macOS {MIN_MACOS} or later"));
        }
        if which::which("container").is_err() {
            return Err("needs Apple's container tool (github.com/apple/container)".into());
        }
        Ok(())
    }

    fn setup(&self) -> Option<Setup> {
        tool_missing().then_some(Setup {
            needs: "Apple's container tool",
            url: RELEASES,
            steps: &[
                "Open the downloaded installer (container-…-installer-signed.pkg) and follow it; it asks for your password.",
                "Then run `container system start` once in Terminal. It may offer to download a Linux kernel: say yes.",
            ],
        })
    }

    fn suits(&self, project: &Path) -> Result<(), String> {
        match needs_macos_tools(project) {
            Some(why) => Err(format!("{why}, which needs macOS tools")),
            None => Ok(()),
        }
    }

    fn prepare(&self, request: &Request, stem: &str) -> anyhow::Result<Prepared> {
        let model = request.model.clone().context("start a model first: OpenCode in the VM needs one to talk to")?;
        let project = request.project.display().to_string();
        anyhow::ensure!(!project.contains(':') && !project.contains(','), "the Linux VM can't share a folder whose path has ':' or ','");
        let container = which::which("container").context("Apple's container tool isn't installed")?;
        run(&container, &["system", "start", "--enable-kernel-install"])?;
        let network = network_name(&container, request.network)?;

        let dir = request.paths.data.join("sandbox");
        let context = dir.join("image");
        write_private(&context.join("Containerfile"), CONTAINERFILE.as_bytes())?;
        let version = opencode_version();
        let image = format!("{IMAGE}:{version}-{RECIPE}");

        // OpenCode's config in the VM: the guard through the relay, and the key it sends.
        let vm = dir.join(format!("vm-{stem}"));
        let (config, state) = (vm.join("config"), vm.join("state"));
        let key = secrets::local_api_key(request.paths)?;
        write_private(&config.join("guard-key"), key.as_bytes())?;
        let setup = opencode::Setup {
            base_url: GUARD_IN_VM.into(),
            api_key_file: PathBuf::from(format!("{HOME}/.config/opencode/guard-key")),
            models: vec![model.clone()],
            default_model: Some(model.alias),
        };
        write_private(&config.join("opencode.json"), opencode::apply("", &setup)?.as_bytes())?;
        crate::paths::private_dir(&state)?;

        let hardware = Hardware::detect();
        let cpus = std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).max(2));
        let memory_gib = (hardware.total_memory >> 30).div_ceil(4).clamp(2, 16);
        let q = |p: &Path| shell_quote(&p.display().to_string());
        let c = q(&container);
        let name = format!("superviser-{stem}");
        let command = format!(
            "{c} system start --enable-kernel-install >/dev/null || exit 1\n\
             if ! {c} image inspect {image} >/dev/null 2>&1; then\n\
             \x20 echo 'First start: building the Linux environment once (a few minutes)…'\n\
             \x20 {c} build --progress plain -t {image} --build-arg OPENCODE_VERSION={version} -f {containerfile} {context} || exit 1\n\
             fi\n\
             {c} stop {name} >/dev/null 2>&1\n\
             SSH_AUTH_SOCK={socket} exec {c} run --rm -it --ssh --name {name} --network {network} --cpus {cpus} --memory {memory_gib}G \\\n\
             \x20 --volume {volume} \\\n\
             \x20 --mount {config_mount} \\\n\
             \x20 --volume {state_volume} \\\n\
             \x20 --workdir /workspace {image}",
            socket = q(request.guard_socket),
            containerfile = q(&context.join("Containerfile")),
            context = q(&context),
            volume = shell_quote(&format!("{project}:/workspace")),
            config_mount = shell_quote(&format!("type=bind,source={},target={HOME}/.config/opencode,readonly", config.display())),
            state_volume = shell_quote(&format!("{}:{HOME}/.local/share/opencode", state.display())),
        );
        Ok(Prepared {
            banner: vec!["OpenCode runs in a Linux VM. Only this folder is shared (as /workspace); the rest of your Mac isn't visible.".into()],
            command,
            files: vec![config.join("opencode.json"), context.join("Containerfile")],
        })
    }
}

/// Runs `container ARGS`, failing with its output.
fn run(container: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new(container).args(args).output().with_context(|| format!("running container {}", args.join(" ")))?;
    anyhow::ensure!(out.status.success(), "container {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The network to attach: the default (NAT) one, or a host-only one when internet is off.
fn network_name(container: &Path, network: Network) -> anyhow::Result<String> {
    if network == Network::Internet {
        return Ok("default".into());
    }
    if run(container, &["network", "inspect", LOCAL_NETWORK]).is_err() {
        run(container, &["network", "create", "--internal", LOCAL_NETWORK])?;
    }
    Ok(LOCAL_NETWORK.into())
}

/// The host's OpenCode version (`v2.3.0` → `2.3.0`), so the VM runs the same one.
fn opencode_version() -> String {
    opencode::version()
        .map(|v| v.trim_start_matches(['v', 'V']).to_string())
        .filter(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || c == '.'))
        .unwrap_or_else(|| DEFAULT_OPENCODE.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apple_projects_need_macos_tools() {
        let root = std::env::temp_dir().join(format!("superviser-markers-{}", std::process::id()));
        let case = |name: &str, files: &[(&str, &str)]| {
            let dir = root.join(name);
            for (path, contents) in files {
                let file = dir.join(path);
                std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                std::fs::write(file, contents).unwrap();
            }
            std::fs::create_dir_all(&dir).unwrap();
            needs_macos_tools(&dir)
        };
        assert!(case("xcode", &[("App/App.xcodeproj/project.pbxproj", "")]).unwrap().contains("Xcode"));
        assert!(case("pods", &[("Podfile", "")]).is_some());
        assert!(case("swift-ios", &[("Package.swift", "platforms: [.iOS(.v17)]")]).is_some());
        assert!(case("swift-server", &[("Package.swift", "let package = Package(name: \"api\")")]).is_none());
        assert!(case("rust", &[("Cargo.toml", ""), ("src/main.rs", "")]).is_none());
        // Markers in dependencies don't count.
        assert!(case("deps", &[("node_modules/x/Podfile", "")]).is_none());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn available_exactly_when_container_is_installed() {
        let installed = which::which("container").is_ok() && cfg!(all(target_os = "macos", target_arch = "aarch64")) && macos_major().is_some_and(|v| v >= MIN_MACOS);
        assert_eq!(LinuxVm.available().is_ok(), installed);
        assert_eq!(LinuxVm.setup().is_some(), !installed && tool_missing());
    }
}
