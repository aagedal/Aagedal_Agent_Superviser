//! The Linux VM backend: OpenCode in a Linux container on Apple's `container` (one
//! lightweight VM per container, on the Virtualization framework), sharing only the
//! project folder. Optional: projects that need macOS tools stay on the macOS sandbox.
//!
//! The image (`Containerfile`: Debian, common build tools and OpenCode at the host's
//! version) is built once per OpenCode version, on first launch: in the Terminal window, or
//! by the chat, which runs `opencode acp` in a VM of its own ([`Vm::command`]).
//! The VM sees the main folder as `/workspace`, the project's other folders under
//! `/folders` (read-only unless the project lets the agent change them), OpenCode's config
//! (read-only) and its own session state; nothing else of the Mac. Blocked programs are
//! left to the guard here. OpenCode reaches models through the guard's
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
use crate::project::Folder;

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
const GUARD_IN_VM: &str = "http://127.0.0.1:8787";
/// Where the main folder appears in the VM.
const WORKSPACE: &str = "/workspace";
/// Where the project's other folders appear in the VM.
const FOLDERS_IN_VM: &str = "/folders";

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
        let vm = Vm::new(request, stem, Use::Terminal)?;
        // OpenCode's config in the VM: the guard through the relay, and the running model.
        let setup = provider_setup(request.guard_path, vec![model.clone()], Some(model.alias));
        let config = vm.dir.join("config").join("opencode.json");
        write_private(&config, opencode::apply("", &setup)?.as_bytes())?;
        let c = shell_quote(&vm.container.display().to_string());
        let quoted = |args: &[String]| args.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ");
        let name = shell_quote(&vm.name(Use::Terminal));
        let command = format!(
            "{c} system start --enable-kernel-install >/dev/null || exit 1\n\
             if ! {c} image inspect {image} >/dev/null 2>&1; then\n\
             \x20 echo 'First start: building the Linux environment once (a few minutes)…'\n\
             \x20 {c} {build} || exit 1\n\
             fi\n\
             {c} stop {name} >/dev/null 2>&1\n\
             SSH_AUTH_SOCK={socket} exec {c} run -it --name {name} {options} {image}",
            image = shell_quote(&vm.image),
            build = quoted(&vm.build_args()),
            socket = shell_quote(&vm.socket.display().to_string()),
            options = quoted(&vm.options),
        );
        let mut banner = vec!["OpenCode runs in a Linux VM. Only this folder is shared (as /workspace); the rest of your Mac isn't visible.".into()];
        banner.extend(vm_folders(request.related).iter().map(|(folder, target)| format!("{} is shared as {target}.", folder.path.display())));
        Ok(Prepared { banner, command, files: vec![config, vm.context.join("Containerfile")] })
    }
}

/// What a VM runs OpenCode for. Each gets its own VM and session state: two VMs can't
/// safely share OpenCode's database through the shared folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Use {
    /// OpenCode's own interface, in a Terminal window.
    Terminal,
    /// `opencode acp`, behind the chat window.
    Chat,
}

/// Names of the chat's VMs start with this.
const CHAT_PREFIX: &str = "superviser-chat-";

/// The OpenCode provider settings for a VM: the guard through the relay, and the key file
/// mounted with OpenCode's config.
pub fn provider_setup(guard_path: &str, models: Vec<opencode::SetupModel>, default_model: Option<String>) -> opencode::Setup {
    opencode::Setup {
        base_url: format!("{GUARD_IN_VM}{guard_path}/v1"),
        api_key_file: PathBuf::from(format!("{HOME}/.config/opencode/guard-key")),
        models,
        default_model,
    }
}

/// A project's VM, ready to run: the image to run (built when missing), what to share and
/// how to reach the guard. The Terminal window and the chat start it the same way.
pub struct Vm {
    pub container: PathBuf,
    pub image: String,
    version: String,
    context: PathBuf,
    /// Its files on the Mac: OpenCode's config and session state.
    dir: PathBuf,
    stem: String,
    /// The guard's socket, forwarded into the VM.
    pub socket: PathBuf,
    /// `container run` options, between `run` and the image, without `--name` or terminal flags.
    pub options: Vec<String>,
}

impl Vm {
    /// Checks the project's folders, starts `container`'s services, and writes the image
    /// recipe and the key OpenCode sends the guard. `request.opencode` is passed in
    /// `OPENCODE_CONFIG_CONTENT`.
    pub fn new(request: &Request, stem: &str, purpose: Use) -> anyhow::Result<Self> {
        anyhow::ensure!(!stem.is_empty() && stem.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')), "bad VM stem");
        for folder in std::iter::once(request.project).chain(request.related.iter().map(|f| f.path.as_path())) {
            let path = folder.display().to_string();
            anyhow::ensure!(!path.contains(':') && !path.contains(','), "the Linux VM can't share {path}: its path has ':' or ','");
        }
        let container = which::which("container").context("Apple's container tool isn't installed")?;
        run(&container, &["system", "start", "--enable-kernel-install"])?;
        let network = network_name(&container, request.network)?;

        let sandbox = request.paths.data.join("sandbox");
        let context = sandbox.join("image");
        write_private(&context.join("Containerfile"), CONTAINERFILE.as_bytes())?;
        let version = opencode_version();
        let image = format!("{IMAGE}:{version}-{RECIPE}");

        let dir = sandbox.join(format!("vm-{stem}"));
        let config = dir.join("config");
        let state = dir.join(match purpose {
            Use::Terminal => "state",
            Use::Chat => "chat-state",
        });
        let key = secrets::local_api_key(request.paths)?;
        write_private(&config.join("guard-key"), key.as_bytes())?;
        crate::paths::private_dir(&state)?;

        let hardware = Hardware::detect();
        let cpus = std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).max(2));
        let memory_gib = (hardware.total_memory >> 30).div_ceil(4).clamp(2, 16);
        let mut options: Vec<String> = ["--rm", "--ssh", "--network", &network, "--cpus", &cpus.to_string(), "--memory", &format!("{memory_gib}G")]
            .map(String::from)
            .into();
        // The VM doesn't mount the host's global opencode.json, so never pass host MCP entries into it.
        let mut opencode_cfg = request.opencode.clone();
        if let Some(obj) = opencode_cfg.as_object_mut() {
            obj.remove("mcp");
        }
        options.extend(["--env".into(), format!("OPENCODE_CONFIG_CONTENT={opencode_cfg}")]);
        options.extend(["--volume".into(), format!("{}:{WORKSPACE}", request.project.display())]);
        for (folder, target) in vm_folders(request.related) {
            let readonly = if folder.writable { "" } else { ",readonly" };
            options.extend(["--mount".into(), format!("type=bind,source={},target={target}{readonly}", folder.path.display())]);
        }
        options.extend(["--mount".into(), format!("type=bind,source={},target={HOME}/.config/opencode,readonly", config.display())]);
        options.extend(["--volume".into(), format!("{}:{HOME}/.local/share/opencode", state.display())]);
        options.extend(["--workdir".into(), WORKSPACE.into()]);
        Ok(Self { container, image, version, context, dir, stem: stem.to_string(), socket: request.guard_socket.to_path_buf(), options })
    }

    /// The container's name for `purpose`.
    pub fn name(&self, purpose: Use) -> String {
        match purpose {
            Use::Terminal => format!("superviser-{}", self.stem),
            Use::Chat => format!("{CHAT_PREFIX}{}", self.stem),
        }
    }

    /// `container` arguments that build the image.
    fn build_args(&self) -> Vec<String> {
        let containerfile = self.context.join("Containerfile").display().to_string();
        let version = format!("OPENCODE_VERSION={}", self.version);
        ["build", "--progress", "plain", "-t", &self.image, "--build-arg", &version, "-f", &containerfile, &self.context.display().to_string()].map(String::from).into()
    }

    pub fn has_image(&self) -> bool {
        Command::new(&self.container).args(["image", "inspect", &self.image]).output().is_ok_and(|out| out.status.success())
    }

    /// The command that builds the image (a few minutes, once per OpenCode version).
    pub fn build_command(&self) -> Command {
        let mut command = Command::new(&self.container);
        command.args(self.build_args());
        command
    }

    /// The command that runs `args` in the VM, its standard input and output the program's
    /// own (no terminal). The VM is named for `purpose`, after stopping one left by that name.
    pub fn command(&self, purpose: Use, args: &[&str]) -> Command {
        let name = self.name(purpose);
        stop(&self.container, &name);
        let mut command = Command::new(&self.container);
        command.args(["run", "-i", "--name", &name]).args(&self.options).arg(&self.image).args(args).env("SSH_AUTH_SOCK", &self.socket);
        command
    }
}

/// Stops the VM called `name`, if it runs. Stopping `container run` alone leaves it running.
pub fn stop(container: &Path, name: &str) {
    let _ = Command::new(container).args(["stop", "--time", "2", name]).output();
}

/// Stops chat VMs an earlier run of the app left (it was killed, or crashed).
pub fn stop_chat_leftovers() {
    let Ok(container) = which::which("container") else { return };
    let Ok(listed) = run(&container, &["list", "--quiet"]) else { return };
    for name in listed.lines().map(str::trim).filter(|n| n.starts_with(CHAT_PREFIX)) {
        tracing::info!("stopping a chat VM left from an earlier run ({name})");
        stop(&container, name);
    }
}

/// The Mac path of `path` as the VM sees it: under `/workspace` (the main folder) or a
/// related folder's place under `/folders`. `None` for other paths or traversal attempts.
pub fn host_path(main: &Path, related: &[Folder], path: &Path) -> Option<PathBuf> {
    let safe_rest = |rest: &Path| {
        rest.components()
            .all(|c| matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir))
    };
    if let Ok(rest) = path.strip_prefix(WORKSPACE) {
        return safe_rest(rest).then(|| main.join(rest));
    }
    vm_folders(related).into_iter().find_map(|(folder, target)| {
        let rest = path.strip_prefix(&target).ok()?;
        safe_rest(rest).then(|| folder.path.join(rest))
    })
}

/// Where the project's folders appear in the VM, main first.
pub fn vm_paths(related: &[Folder]) -> (String, Vec<String>) {
    (WORKSPACE.into(), vm_folders(related).into_iter().map(|(_, target)| target).collect())
}

/// Where each of `related` appears in the VM: `/folders/<name>`, numbered when names repeat.
fn vm_folders(related: &[Folder]) -> Vec<(&Folder, String)> {
    let mut used: Vec<String> = Vec::new();
    related
        .iter()
        .map(|folder| {
            let base = folder.path.file_name().map_or("folder".into(), |n| n.to_string_lossy().into_owned());
            let mut name = base.clone();
            let mut n = 2;
            while used.contains(&name) {
                name = format!("{base}-{n}");
                n += 1;
            }
            used.push(name.clone());
            (folder, format!("{FOLDERS_IN_VM}/{name}"))
        })
        .collect()
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
    fn related_folders_get_distinct_names() {
        let folder = |p: &str| Folder { path: p.into(), writable: false };
        let related = [folder("/a/docs"), folder("/b/docs"), folder("/c/api")];
        let names: Vec<String> = vm_folders(&related).into_iter().map(|(_, t)| t).collect();
        assert_eq!(names, ["/folders/docs", "/folders/docs-2", "/folders/api"]);
    }

    #[test]
    fn vm_paths_map_back_to_the_mac() {
        let related = [Folder { path: "/a/docs".into(), writable: false }, Folder { path: "/b/docs".into(), writable: true }];
        let main = Path::new("/Users/a/app");
        let host = |p: &str| host_path(main, &related, Path::new(p));
        assert_eq!(host("/workspace/src/main.rs"), Some(PathBuf::from("/Users/a/app/src/main.rs")));
        assert_eq!(host("/folders/docs/a.md"), Some(PathBuf::from("/a/docs/a.md")));
        assert_eq!(host("/folders/docs-2/b.md"), Some(PathBuf::from("/b/docs/b.md")));
        assert_eq!(host("/workspaces/x"), None);
        assert_eq!(host("/workspace/../../etc/passwd"), None);
        assert_eq!(host("/folders/docs/../secret"), None);
        assert_eq!(host("/etc/passwd"), None);
        assert_eq!(vm_paths(&related), ("/workspace".into(), vec!["/folders/docs".into(), "/folders/docs-2".into()]));
    }

    #[test]
    fn available_exactly_when_container_is_installed() {
        let installed = which::which("container").is_ok() && cfg!(all(target_os = "macos", target_arch = "aarch64")) && macos_major().is_some_and(|v| v >= MIN_MACOS);
        assert_eq!(LinuxVm.available().is_ok(), installed);
        assert_eq!(LinuxVm.setup().is_some(), !installed && tool_missing());
    }
}
