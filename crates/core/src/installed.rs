//! Models on disk: `models/<owner>/<repo>/<quant>/`, each with a `manifest.json` written once
//! every file is downloaded and verified.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::catalog::CatalogModel;
use crate::hub::{self, Hub};
use crate::paths::Paths;
use crate::quant;

const MANIFEST: &str = "manifest.json";

/// Who put a model on this machine. Only the app's own models may be deleted from here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    #[default]
    App,
    /// The shared Hugging Face cache: Unsloth Studio, `hf download`, newer llama.cpp.
    HuggingFace,
    LmStudio,
    Ollama,
    LlamaCpp,
    Omlx,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::App => "Agent Superviser",
            Source::HuggingFace => "Hugging Face cache",
            Source::LmStudio => "LM Studio",
            Source::Ollama => "Ollama",
            Source::LlamaCpp => "llama.cpp",
            Source::Omlx => "oMLX",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledModel {
    /// `owner/repo:QUANT` for Hub models (from any source), `ollama:name:tag` for Ollama.
    pub id: String,
    /// What harnesses call it: `qwen3.5-9b`.
    pub alias: String,
    pub model: CatalogModel,
    pub installed_at: String,
    #[serde(default)]
    pub source: Source,
    #[serde(skip)]
    pub dir: PathBuf,
}

impl InstalledModel {
    /// The file llama.cpp loads; it finds the other shards next to it.
    pub fn entry_path(&self) -> Option<PathBuf> {
        self.model.quant.entry_file().map(|f| self.dir.join(quant::local_path(&f.path)))
    }
}

pub fn model_id(model: &CatalogModel) -> String {
    format!("{}:{}", model.repo, model.quant.label)
}

/// `unsloth/Qwen3.5-9B-GGUF` → `qwen3.5-9b`.
pub fn alias(repo: &str) -> String {
    let name = repo.rsplit('/').next().unwrap_or(repo);
    let name = name.strip_suffix("-GGUF").or_else(|| name.strip_suffix("-gguf")).unwrap_or(name);
    name.to_ascii_lowercase()
}

fn model_dir(paths: &Paths, model: &CatalogModel) -> anyhow::Result<PathBuf> {
    validate_model(model)?;
    let root = paths.models();
    let mut dir = root.clone();
    for part in model.repo.split('/') {
        dir.push(sanitize(part));
    }
    dir.push(sanitize(&model.quant.label));
    anyhow::ensure!(dir.starts_with(&root) && dir != root, "unsafe model directory {}", dir.display());
    Ok(dir)
}

/// Checks that `model`'s repo, quant label and file paths cannot traverse out of its install folder.
pub fn validate_model(model: &CatalogModel) -> anyhow::Result<()> {
    let bad_segment = |s: &str| s.is_empty() || s == "." || s == ".." || s.contains('\\');
    anyhow::ensure!(!model.repo.is_empty() && !model.repo.split('/').any(bad_segment), "unsafe model repo {}", model.repo);
    anyhow::ensure!(!bad_segment(&model.quant.label) && !model.quant.label.contains('/'), "unsafe quant label {}", model.quant.label);
    for file in &model.quant.files {
        let p = Path::new(&file.path);
        anyhow::ensure!(
            !file.path.is_empty()
                && !file.path.contains('\\')
                && p.components().all(|c| matches!(c, std::path::Component::Normal(_))),
            "unsafe file path {}",
            file.path
        );
    }
    Ok(())
}

fn sanitize(part: &str) -> String {
    let cleaned: String = part.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." { "_".into() } else { cleaned }
}

/// Every complete model, newest first.
pub fn list(paths: &Paths) -> Vec<InstalledModel> {
    let mut found = Vec::new();
    let mut visited = std::collections::HashSet::new();
    let mut stack = vec![(paths.models(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > 16 {
            continue;
        }
        if let Ok(real) = std::fs::canonicalize(&dir)
            && !visited.insert(real)
        {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push((path, depth + 1));
            } else if path.file_name().is_some_and(|n| n == MANIFEST) {
                match read_manifest(&path) {
                    Ok(model) => found.push(model),
                    Err(e) => tracing::warn!("skipping {}: {e:#}", path.display()),
                }
            }
        }
    }
    found.sort_by(|a, b| b.installed_at.cmp(&a.installed_at));
    found
}

fn read_manifest(path: &Path) -> anyhow::Result<InstalledModel> {
    let mut model: InstalledModel = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    model.dir = path.parent().unwrap().to_path_buf();
    Ok(model)
}

pub fn find(paths: &Paths, id: &str) -> Option<InstalledModel> {
    list(paths).into_iter().find(|m| m.id == id)
}

pub fn remove(paths: &Paths, id: &str) -> anyhow::Result<()> {
    let model = find(paths, id).with_context(|| format!("{id} is not installed"))?;
    // Only ever delete inside the models directory.
    anyhow::ensure!(model.dir.starts_with(paths.models()), "refusing to delete {}", model.dir.display());
    std::fs::remove_dir_all(&model.dir)?;
    Ok(())
}

/// Download progress over all of a model's files.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Progress {
    pub done: u64,
    pub total: u64,
}

/// Downloads every file of `model`, verifying each, then writes the manifest.
pub async fn install(
    hub: &Hub,
    paths: &Paths,
    model: &CatalogModel,
    cancel: Arc<AtomicBool>,
    progress: impl FnMut(Progress),
) -> anyhow::Result<InstalledModel> {
    let dir = model_dir(paths, model)?;
    install_to(hub, &dir, model, cancel, progress).await
}

/// The judge's model: kept in its own folder, so it isn't listed with the chat models.
pub fn judge_dir(paths: &Paths, model: &CatalogModel) -> PathBuf {
    paths.judge_model().join(sanitize(&model_id(model)))
}

/// The judge's model, if this version of it is downloaded.
pub fn find_judge(paths: &Paths, model: &CatalogModel) -> Option<InstalledModel> {
    read_manifest(&judge_dir(paths, model).join(MANIFEST)).ok().filter(|m| m.model.revision == model.revision)
}

/// [`install`] into `dir`.
pub async fn install_to(
    hub: &Hub,
    dir: &Path,
    model: &CatalogModel,
    cancel: Arc<AtomicBool>,
    mut progress: impl FnMut(Progress),
) -> anyhow::Result<InstalledModel> {
    validate_model(model)?;
    let dir = dir.to_path_buf();
    if let Some(free) = available_space(&dir) {
        check_space(&model.name, missing_bytes(&dir, model), free)?;
    }
    let total = model.quant.size;
    let mut finished = 0u64;
    for file in &model.quant.files {
        let dest = dir.join(quant::local_path(&file.path));
        anyhow::ensure!(dest.starts_with(&dir) && !file.path.contains(".."), "unsafe file path {}", file.path);
        if std::fs::metadata(&dest).is_ok_and(|m| m.len() == file.size) {
            if let Some(expected) = &file.sha256 {
                if verify_sha256(&dest, expected, &cancel).await? {
                    finished += file.size;
                    continue;
                }
                let _ = tokio::fs::remove_file(&dest).await;
            } else {
                finished += file.size;
                continue;
            }
        }
        hub.download(&model.repo, &model.revision, file, &dest, &cancel, |done| {
            progress(Progress { done: finished + done, total })
        })
        .await?;
        finished += file.size;
    }
    let installed = InstalledModel {
        id: model_id(model),
        alias: alias(&model.repo),
        model: model.clone(),
        installed_at: time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339)?,
        source: Source::App,
        dir: dir.clone(),
    };
    std::fs::write(dir.join(MANIFEST), serde_json::to_string_pretty(&installed)?)?;
    progress(Progress { done: total, total });
    Ok(installed)
}

async fn verify_sha256(path: &Path, expected: &str, cancel: &Arc<AtomicBool>) -> anyhow::Result<bool> {
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            anyhow::bail!("download cancelled");
        }
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()).eq_ignore_ascii_case(expected))
}

/// Room a download must leave, so it never fills the disk to the last byte.
const SPARE_DISK: u64 = 1 << 30;

/// Bytes of `model` still to download into `dir`: files not there yet, less what earlier
/// attempts left in `.part` files.
fn missing_bytes(dir: &Path, model: &CatalogModel) -> u64 {
    model
        .quant
        .files
        .iter()
        .map(|file| {
            let dest = dir.join(quant::local_path(&file.path));
            let have = match std::fs::metadata(&dest) {
                Ok(m) if m.len() == file.size => file.size,
                _ => std::fs::metadata(hub::part_path(&dest)).map_or(0, |m| m.len()),
            };
            file.size.saturating_sub(have)
        })
        .sum()
}

/// Refuses a download that wouldn't fit, before it starts rather than when the disk is full.
fn check_space(name: &str, missing: u64, free: u64) -> anyhow::Result<()> {
    if missing > 0 && missing + SPARE_DISK > free {
        let gb = |bytes: u64| bytes as f64 / 1e9;
        anyhow::bail!("Not enough disk space for {name}: it needs {:.1} GB more, and {:.1} GB is free", gb(missing + SPARE_DISK), gb(free));
    }
    Ok(())
}

/// Bytes free at `dir` (or the nearest folder above it that exists) for files the user asked
/// for: on macOS this counts the purgeable space the system frees on demand, as Finder does,
/// which `statvfs` leaves out. `None` where we can't tell.
pub(crate) fn available_space(dir: &Path) -> Option<u64> {
    let existing = dir.ancestors().find(|d| d.is_dir())?;
    #[cfg(target_os = "macos")]
    {
        use objc2_foundation::{NSNumber, NSString, NSURL, NSURLVolumeAvailableCapacityForImportantUsageKey};
        let url = NSURL::fileURLWithPath(&NSString::from_str(existing.to_str()?));
        let mut value = None;
        // SAFETY: the key is a Foundation constant, and the value it names is an NSNumber.
        unsafe { url.getResourceValue_forKey_error(&mut value, NSURLVolumeAvailableCapacityForImportantUsageKey) }.ok()?;
        let number = value?.downcast::<NSNumber>().ok()?;
        u64::try_from(number.longLongValue()).ok()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = existing;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_disk_space_for_what_is_left() {
        let tmp = std::env::temp_dir().join(format!("superviser-space-{}", std::process::id()));
        let model = crate::catalog::Catalog::bundled().models.remove(0);
        let file = &model.quant.files[0];
        assert_eq!(missing_bytes(&tmp, &model), model.quant.size);
        let dest = tmp.join(quant::local_path(&file.path));
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(hub::part_path(&dest), vec![0u8; 1000]).unwrap();
        assert_eq!(missing_bytes(&tmp, &model), model.quant.size - 1000);
        std::fs::remove_dir_all(&tmp).unwrap();

        assert!(check_space("m", 10 << 30, 12 << 30).is_ok());
        let err = check_space("m", 10 << 30, 10 << 30).unwrap_err().to_string();
        assert_eq!(err, "Not enough disk space for m: it needs 11.8 GB more, and 10.7 GB is free");
        assert!(check_space("m", 0, 0).is_ok());
        #[cfg(target_os = "macos")]
        assert!(available_space(&std::env::temp_dir().join("not/there/yet")).is_some_and(|b| b > 0));
    }

    #[test]
    fn aliases() {
        assert_eq!(alias("unsloth/Qwen3.5-9B-GGUF"), "qwen3.5-9b");
        assert_eq!(alias("unsloth/gpt-oss-20b-GGUF"), "gpt-oss-20b");
    }

    #[test]
    fn lists_and_removes_manifests() {
        let tmp = std::env::temp_dir().join(format!("superviser-installed-{}", std::process::id()));
        let paths = Paths::at(&tmp);
        let model = crate::catalog::Catalog::bundled().models.remove(0);
        let dir = model_dir(&paths, &model).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let installed = InstalledModel {
            id: model_id(&model),
            alias: alias(&model.repo),
            model,
            installed_at: "2026-11-17T00:00:00Z".into(),
            source: Source::App,
            dir: dir.clone(),
        };
        std::fs::write(dir.join(MANIFEST), serde_json::to_string(&installed).unwrap()).unwrap();
        let listed = list(&paths);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].dir, dir);
        remove(&paths, &installed.id).unwrap();
        assert!(list(&paths).is_empty());
        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[test]
    fn rejects_traversal_in_model_metadata() {
        let paths = Paths::at("/tmp/superviser-models-test");
        let mut bad_repo = crate::catalog::Catalog::bundled().models.remove(0);
        bad_repo.repo = "../escape/repo".into();
        assert!(model_dir(&paths, &bad_repo).is_err());

        let mut bad_quant = crate::catalog::Catalog::bundled().models.remove(0);
        bad_quant.quant.label = "..".into();
        assert!(model_dir(&paths, &bad_quant).is_err());

        let mut bad_file = crate::catalog::Catalog::bundled().models.remove(0);
        bad_file.quant.files[0].path = "../pwn.gguf".into();
        assert!(validate_model(&bad_file).is_err());
    }
}
