//! Fetches the bundled programs (`sidecars.json`) for the build target into `binaries/`,
//! verified by SHA-256, before Tauri bundles them. Already fetched ones are kept.

use std::path::Path;
use std::process::Command;

use sha2::{Digest, Sha256};

fn main() {
    let target = std::env::var("TARGET").expect("cargo sets TARGET");
    if target.ends_with("apple-darwin") {
        fetch_sidecars(&target);
    }
    tauri_build::build();
}

fn fetch_sidecars(target: &str) {
    println!("cargo:rerun-if-changed=sidecars.json");
    let pins: serde_json::Value = serde_json::from_str(&std::fs::read_to_string("sidecars.json").expect("sidecars.json")).expect("sidecars.json is JSON");
    let out = std::env::var("OUT_DIR").expect("cargo sets OUT_DIR");
    std::fs::create_dir_all("binaries").expect("binaries/");
    for (name, pin) in pins.as_object().unwrap().iter().filter(|(k, _)| !k.starts_with('_')) {
        let Some(build) = pin.get(target) else { panic!("sidecars.json has no {name} for {target}") };
        let (url, sha256, member) = (build["url"].as_str().unwrap(), build["sha256"].as_str().unwrap(), build["member"].as_str().unwrap());
        let dest = Path::new("binaries").join(format!("{name}-{target}"));
        // The archive's checksum, kept beside the program, says which pin it came from.
        let marker = Path::new("binaries").join(format!(".{name}-{target}.sha256"));
        println!("cargo:rerun-if-changed={}", dest.display());
        if dest.is_file() && std::fs::read_to_string(&marker).is_ok_and(|m| m.trim() == sha256) {
            continue;
        }
        let archive = Path::new(&out).join(url.rsplit('/').next().unwrap());
        run(Command::new("curl").args(["--fail", "--location", "--silent", "--show-error", "--output"]).arg(&archive).arg(url), &format!("downloading {url}"));
        let digest = hex(&Sha256::digest(std::fs::read(&archive).expect("the download")));
        assert_eq!(digest, sha256, "{url} doesn't match its pinned SHA-256 in sidecars.json");
        let unpacked = Path::new(&out).join(format!("{name}-unpacked"));
        let _ = std::fs::remove_dir_all(&unpacked);
        std::fs::create_dir_all(&unpacked).unwrap();
        if url.ends_with(".zip") {
            run(Command::new("unzip").arg("-q").arg(&archive).arg(member).arg("-d").arg(&unpacked), "unzipping");
        } else {
            run(Command::new("tar").arg("-xzf").arg(&archive).arg("-C").arg(&unpacked).arg(member), "unpacking");
        }
        std::fs::copy(unpacked.join(member), &dest).expect("copying the program");
        #[cfg(unix)]
        std::fs::set_permissions(&dest, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        std::fs::write(&marker, sha256).unwrap();
    }
}

fn run(command: &mut Command, what: &str) {
    let status = command.status().unwrap_or_else(|e| panic!("{what}: {e}"));
    assert!(status.success(), "{what} failed");
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
