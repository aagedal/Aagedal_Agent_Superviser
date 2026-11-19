//! Models on disk: `models/<owner>/<repo>/<quant>/`, each with a `manifest.json` written once
//! every file is downloaded and verified.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::catalog::CatalogModel;
use crate::hub::Hub;
use crate::paths::Paths;

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
        self.model.quant.entry_file().map(|f| self.dir.join(&f.path))
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

fn model_dir(paths: &Paths, model: &CatalogModel) -> PathBuf {
    let mut dir = paths.models();
    for part in model.repo.split('/') {
        dir.push(sanitize(part));
    }
    dir.push(sanitize(&model.quant.label));
    dir
}

fn sanitize(part: &str) -> String {
    part.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect()
}

/// Every complete model, newest first.
pub fn list(paths: &Paths) -> Vec<InstalledModel> {
    let mut found = Vec::new();
    let mut stack = vec![paths.models()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
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
    install_to(hub, &model_dir(paths, model), model, cancel, progress).await
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
    let dir = dir.to_path_buf();
    let total = model.quant.size;
    let mut finished = 0u64;
    for file in &model.quant.files {
        let dest = dir.join(&file.path);
        anyhow::ensure!(dest.starts_with(&dir) && !file.path.contains(".."), "unsafe file path {}", file.path);
        if std::fs::metadata(&dest).is_ok_and(|m| m.len() == file.size) {
            finished += file.size;
            continue;
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let dir = model_dir(&paths, &model);
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
}
