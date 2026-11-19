//! Fetches the bundled programs (`sidecars.json`) for the build target into `binaries/`,
//! verified by SHA-256, before Tauri bundles them. Already fetched ones are kept.

use std::collections::HashSet;
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
        // A program alone is named for its target (Tauri's externalBin); a folder (a resource)
        // holds one target's build at a time.
        let folder = pin.get("folder").and_then(|f| f.as_str());
        let (dest, marker) = match folder {
            Some(folder) => (Path::new("binaries").join(folder), Path::new("binaries").join(format!(".{folder}.sha256"))),
            None => (Path::new("binaries").join(format!("{name}-{target}")), Path::new("binaries").join(format!(".{name}-{target}.sha256"))),
        };
        // The archive's checksum, kept beside the program, says which pin it came from.
        println!("cargo:rerun-if-changed={}", dest.display());
        if dest.exists() && std::fs::read_to_string(&marker).is_ok_and(|m| m.trim() == sha256) {
            continue;
        }
        let archive = Path::new(&out).join(url.rsplit('/').next().unwrap());
        run(Command::new("curl").args(["--fail", "--location", "--silent", "--show-error", "--output"]).arg(&archive).arg(url), &format!("downloading {url}"));
        let digest = hex(&Sha256::digest(std::fs::read(&archive).expect("the download")));
        assert_eq!(digest, sha256, "{url} doesn't match its pinned SHA-256 in sidecars.json");
        let unpacked = Path::new(&out).join(format!("{name}-unpacked"));
        let _ = std::fs::remove_dir_all(&unpacked);
        std::fs::create_dir_all(&unpacked).unwrap();
        // A folder needs the libraries beside the program too, so it gets the whole archive.
        let members: &[&str] = if folder.is_some() { &[] } else { &[member] };
        if url.ends_with(".zip") {
            run(Command::new("unzip").arg("-q").arg(&archive).args(members).arg("-d").arg(&unpacked), "unzipping");
        } else {
            run(Command::new("tar").arg("-xzf").arg(&archive).arg("-C").arg(&unpacked).args(members), "unpacking");
        }
        if folder.is_some() {
            copy_with_libraries(&unpacked.join(member), &dest);
        } else {
            std::fs::copy(unpacked.join(member), &dest).expect("copying the program");
            #[cfg(unix)]
            std::fs::set_permissions(&dest, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        }
        std::fs::write(&marker, sha256).unwrap();
    }
}

/// Copies `program` into `dest` (emptied first) with the libraries it loads through `@rpath`,
/// which its `@loader_path` rpath finds beside it, and the license beside it. Copies are
/// unmodified, so they keep their signatures.
fn copy_with_libraries(program: &Path, dest: &Path) {
    let source = program.parent().unwrap();
    let _ = std::fs::remove_dir_all(dest);
    std::fs::create_dir_all(dest).unwrap();
    let mut queue = vec![program.file_name().unwrap().to_string_lossy().into_owned()];
    let mut copied = HashSet::new();
    while let Some(file) = queue.pop() {
        if !copied.insert(file.clone()) {
            continue;
        }
        // `fs::copy` follows symlinks: `libggml.1.dylib` arrives as the file it names.
        std::fs::copy(source.join(&file), dest.join(&file)).unwrap_or_else(|e| panic!("copying {file}: {e}"));
        let out = Command::new("otool").arg("-L").arg(source.join(&file)).output().expect("otool (Xcode's command line tools)");
        assert!(out.status.success(), "otool -L {file} failed");
        let linked = String::from_utf8_lossy(&out.stdout);
        queue.extend(linked.lines().filter_map(|l| l.trim().strip_prefix("@rpath/")?.split(' ').next().map(str::to_string)));
    }
    if source.join("LICENSE").is_file() {
        std::fs::copy(source.join("LICENSE"), dest.join("LICENSE")).unwrap();
    }
}

fn run(command: &mut Command, what: &str) {
    let status = command.status().unwrap_or_else(|e| panic!("{what}: {e}"));
    assert!(status.success(), "{what} failed");
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
