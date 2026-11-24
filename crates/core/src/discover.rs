//! Finds models other apps already downloaded, so they don't have to be downloaded again.
//!
//! Files are used where they are: nothing is copied, moved or deleted. Each GGUF header is
//! read, so found models go through the same rules as downloads (4-bit or better, a chat
//! model with enough context, memory fit at start). MLX models are listed but can't run yet.
//!
//! | Source | Layout |
//! |---|---|
//! | Hugging Face cache (Unsloth Studio, `hf`, newer llama.cpp) | `models--OWNER--REPO/snapshots/REV/…` |
//! | LM Studio | `models/PUBLISHER/REPO/…` |
//! | Ollama | `manifests/REGISTRY/NAMESPACE/MODEL/TAG` → `blobs/sha256-…` |
//! | llama.cpp (`-hf`, older versions) | flat `OWNER_REPO_FILE.gguf` |
//! | oMLX | MLX folders |

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;

use crate::catalog::{CatalogModel, MIN_CONTEXT, display_name};
use crate::hub::{QuantGroup, RemoteFile};
use crate::installed::{self, InstalledModel, Source};
use crate::{gguf, quant};

/// Names of models that aren't chat models: speech, embeddings, image generation.
static NOT_CHAT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)asr|whisper|parakeet|canary|wav2vec|embed|bge|rerank|tts|vocab|clip|siglip|diffusion|flux").unwrap()
});

/// Where each app keeps its models. Only directories that exist are kept.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Locations {
    pub hf_hub: Vec<PathBuf>,
    pub lmstudio: Vec<PathBuf>,
    pub ollama: Vec<PathBuf>,
    pub llamacpp: Vec<PathBuf>,
    pub omlx: Vec<PathBuf>,
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn existing(paths: Vec<Option<PathBuf>>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for path in paths.into_iter().flatten() {
        if path.is_dir() && !out.contains(&path) {
            out.push(path);
        }
    }
    out
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

impl Locations {
    /// The default locations on this OS, honouring each tool's environment variables.
    pub fn detect() -> Self {
        let Some(base) = directories::BaseDirs::new() else { return Self::default() };
        let home = base.home_dir().to_path_buf();
        let xdg_cache = env_path("XDG_CACHE_HOME").unwrap_or_else(|| home.join(".cache"));

        let hf_home = env_path("HF_HOME").unwrap_or_else(|| xdg_cache.join("huggingface"));
        let hf_hub = existing(vec![env_path("HF_HUB_CACHE"), env_path("HUGGINGFACE_HUB_CACHE"), Some(hf_home.join("hub"))]);

        // LM Studio can move its home; `~/.lmstudio-home-pointer` holds the path.
        let lms_home = std::fs::read_to_string(home.join(".lmstudio-home-pointer"))
            .ok()
            .map(|p| PathBuf::from(p.trim()))
            .unwrap_or_else(|| home.join(".lmstudio"));
        let lms_custom = read_json(&lms_home.join("settings.json"))
            .and_then(|s| s.get("downloadsFolder").and_then(|v| v.as_str()).map(PathBuf::from));
        let lmstudio = existing(vec![lms_custom, Some(lms_home.join("models")), Some(xdg_cache.join("lm-studio").join("models"))]);

        let ollama = existing(vec![
            env_path("OLLAMA_MODELS"),
            Some(home.join(".ollama").join("models")),
            cfg!(target_os = "linux").then(|| PathBuf::from("/usr/share/ollama/.ollama/models")),
        ]);

        let llamacpp = existing(vec![
            env_path("LLAMA_CACHE"),
            cfg!(target_os = "macos").then(|| home.join("Library").join("Caches").join("llama.cpp")),
            cfg!(target_os = "linux").then(|| xdg_cache.join("llama.cpp")),
            cfg!(windows).then(|| base.data_local_dir().join("llama.cpp")),
        ]);

        let omlx_settings = read_json(&home.join(".omlx").join("settings.json"));
        let mut omlx: Vec<Option<PathBuf>> = omlx_settings
            .as_ref()
            .and_then(|s| s.pointer("/model/model_dirs"))
            .and_then(|v| v.as_array())
            .map(|dirs| dirs.iter().filter_map(|d| d.as_str().map(|d| Some(PathBuf::from(d)))).collect())
            .unwrap_or_default();
        omlx.push(Some(home.join(".omlx").join("models")));

        Self { hf_hub, lmstudio, ollama, llamacpp, omlx: existing(omlx) }
    }
}

/// A file or folder we found but won't offer, and why.
#[derive(Debug, Clone, Serialize)]
pub struct Skipped {
    pub source: Source,
    pub name: String,
    pub path: PathBuf,
    pub reason: String,
}

/// An MLX model: found, but not runnable until MLX support lands.
#[derive(Debug, Clone, Serialize)]
pub struct MlxModel {
    pub source: Source,
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Discovery {
    pub models: Vec<InstalledModel>,
    pub skipped: Vec<Skipped>,
    pub mlx: Vec<MlxModel>,
}

/// One model's files, before its header is read.
struct Candidate {
    source: Source,
    repo: Option<String>,
    name: String,
    /// Set when the id isn't `repo:LABEL`.
    id: Option<String>,
    dir: PathBuf,
    /// Relative to `dir`.
    files: Vec<String>,
    revision: String,
}

pub fn scan(locations: &Locations) -> Discovery {
    let mut discovery = Discovery::default();
    let mut candidates = Vec::new();
    for root in &locations.hf_hub {
        scan_hf_cache(root, &mut candidates, &mut discovery);
    }
    for root in &locations.lmstudio {
        scan_publisher_tree(Source::LmStudio, root, &mut candidates, &mut discovery);
    }
    for root in &locations.ollama {
        scan_ollama(root, &mut candidates);
    }
    for root in &locations.llamacpp {
        for (files, _) in gguf_groups(root, 1, Source::LlamaCpp, &mut discovery) {
            let stem = group_stem(&files[0]);
            candidates.push(Candidate {
                source: Source::LlamaCpp,
                repo: None,
                id: Some(format!("llamacpp:{stem}")),
                name: stem,
                dir: root.clone(),
                files,
                revision: String::new(),
            });
        }
    }
    for root in &locations.omlx {
        for dir in subdirs(root).into_iter().flat_map(|d| std::iter::once(d.clone()).chain(subdirs(&d))) {
            if is_mlx_dir(&dir) {
                let name = dir.strip_prefix(root).unwrap_or(&dir).display().to_string();
                discovery.mlx.push(MlxModel { source: Source::Omlx, name, path: dir });
            }
        }
    }

    for candidate in candidates {
        let (source, name, path) = (candidate.source, candidate.name.clone(), candidate.dir.join(&candidate.files[0]));
        match evaluate(candidate) {
            Ok(model) if !discovery.models.iter().any(|m| m.id == model.id) => discovery.models.push(model),
            Ok(_) => {}
            Err(reason) => discovery.skipped.push(Skipped { source, name, path, reason }),
        }
    }
    discovery.models.sort_by_key(|m| m.model.name.to_lowercase());
    discovery
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    dirs.sort();
    dirs
}

fn is_gguf(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("gguf"))
}

/// The file stem without quant-independent shard suffix: what shards of one model share.
fn group_stem(rel: &str) -> String {
    let file = rel.rsplit('/').next().unwrap_or(rel);
    let stem = file.strip_suffix(".gguf").or_else(|| file.strip_suffix(".GGUF")).unwrap_or(file);
    match quant::shard(file) {
        Some(_) => stem.rsplitn(4, '-').nth(3).unwrap_or(stem).to_string(),
        None => stem.to_string(),
    }
}

/// GGUF models under `dir` (to `depth` levels), shards grouped, with whether each is
/// complete. Incomplete groups are reported as skipped and left out.
fn gguf_groups(dir: &Path, depth: usize, source: Source, discovery: &mut Discovery) -> Vec<(Vec<String>, PathBuf)> {
    let mut files = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), 0)];
    while let Some((current, level)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            // `is_file` follows symlinks (the Hugging Face cache) and is false when one dangles.
            if path.is_dir() && level + 1 < depth {
                stack.push((path, level + 1));
            } else if path.is_file() && is_gguf(&path) {
                let rel = path.strip_prefix(dir).unwrap().to_string_lossy().replace('\\', "/");
                if !quant::is_auxiliary(&rel) {
                    files.push(rel);
                }
            }
        }
    }
    let mut groups: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for rel in files {
        let parent = rel.rsplit_once('/').map_or("", |(p, _)| p).to_string();
        groups.entry((parent, group_stem(&rel))).or_default().push(rel);
    }
    let mut out = Vec::new();
    for ((_, stem), mut files) in groups {
        files.sort();
        let shards: Vec<(u32, u32)> = files.iter().filter_map(|f| quant::shard(f)).collect();
        if let Some(&(_, total)) = shards.first() {
            let indices: HashSet<u32> = shards.iter().map(|s| s.0).collect();
            if (1..=total).any(|i| !indices.contains(&i)) {
                let reason = format!("incomplete download ({} of {total} parts)", indices.len());
                discovery.skipped.push(Skipped { source, name: stem, path: dir.join(&files[0]), reason });
                continue;
            }
        }
        out.push((files, dir.to_path_buf()));
    }
    out
}

fn is_mlx_dir(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else { return false };
    let names: Vec<String> = entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    names.iter().any(|n| n == "config.json") && names.iter().any(|n| n.ends_with(".safetensors"))
}

fn looks_mlx(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.starts_with("mlx-community/") || lower.contains("mlx")
}

fn scan_hf_cache(root: &Path, candidates: &mut Vec<Candidate>, discovery: &mut Discovery) {
    for repo_dir in subdirs(root) {
        let Some(name) = repo_dir.file_name().and_then(|n| n.to_str()) else { continue };
        let Some(encoded) = name.strip_prefix("models--") else { continue };
        let repo = encoded.replacen("--", "/", 1);
        // Newest snapshot first, so each quant comes from the latest revision that has it.
        let mut snapshots = subdirs(&repo_dir.join("snapshots"));
        snapshots.sort_by_key(|s| std::cmp::Reverse(std::fs::metadata(s).and_then(|m| m.modified()).ok()));
        let mut seen = HashSet::new();
        let mut any_gguf = false;
        for snapshot in &snapshots {
            for (files, dir) in gguf_groups(snapshot, 3, Source::HuggingFace, discovery) {
                any_gguf = true;
                if !seen.insert(group_stem(&files[0])) {
                    continue;
                }
                let revision = snapshot.file_name().unwrap().to_string_lossy().into_owned();
                candidates.push(Candidate { source: Source::HuggingFace, repo: Some(repo.clone()), name: display_name(&repo), id: None, dir, files, revision });
            }
        }
        if !any_gguf
            && looks_mlx(&repo)
            && let Some(snapshot) = snapshots.iter().find(|s| is_mlx_dir(s))
        {
            discovery.mlx.push(MlxModel { source: Source::HuggingFace, name: repo, path: snapshot.clone() });
        }
    }
}

/// LM Studio's `PUBLISHER/REPO/…` layout.
fn scan_publisher_tree(source: Source, root: &Path, candidates: &mut Vec<Candidate>, discovery: &mut Discovery) {
    for publisher in subdirs(root) {
        for repo_dir in subdirs(&publisher) {
            let repo = format!(
                "{}/{}",
                publisher.file_name().unwrap().to_string_lossy(),
                repo_dir.file_name().unwrap().to_string_lossy()
            );
            let groups = gguf_groups(&repo_dir, 2, source, discovery);
            if groups.is_empty() && is_mlx_dir(&repo_dir) {
                discovery.mlx.push(MlxModel { source, name: repo.clone(), path: repo_dir.clone() });
            }
            for (files, dir) in groups {
                candidates.push(Candidate { source, repo: Some(repo.clone()), name: display_name(&repo), id: None, dir, files, revision: String::new() });
            }
        }
    }
}

/// Ollama: each manifest names its model layer, a GGUF blob.
fn scan_ollama(root: &Path, candidates: &mut Vec<Candidate>) {
    let manifests = root.join("manifests");
    let mut visited = std::collections::HashSet::new();
    let mut stack = vec![(manifests.clone(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > 8 {
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
                continue;
            }
            let Ok(rel) = path.strip_prefix(&manifests) else { continue };
            let parts: Vec<String> = rel.iter().map(|p| p.to_string_lossy().into_owned()).collect();
            let [registry, namespace, model, tag] = parts.as_slice() else { continue };
            let Some(manifest) = read_json(&path) else { continue };
            let Some(digest) = manifest["layers"].as_array().and_then(|layers| {
                layers
                    .iter()
                    .find(|l| l["mediaType"] == "application/vnd.ollama.image.model")
                    .and_then(|l| l["digest"].as_str())
            }) else {
                continue;
            };
            let blob = digest.replace(':', "-");
            if blob.is_empty() || blob == "." || blob == ".." || blob.contains('/') || blob.contains('\\') {
                continue;
            }
            let name = match (registry.as_str(), namespace.as_str()) {
                ("registry.ollama.ai", "library") => format!("{model}:{tag}"),
                ("registry.ollama.ai", _) => format!("{namespace}/{model}:{tag}"),
                _ => format!("{registry}/{namespace}/{model}:{tag}"),
            };
            let repo = matches!(registry.as_str(), "hf.co" | "huggingface.co").then(|| format!("{namespace}/{model}"));
            candidates.push(Candidate {
                source: Source::Ollama,
                repo,
                id: Some(format!("ollama:{name}")),
                name,
                dir: root.join("blobs"),
                files: vec![blob],
                revision: String::new(),
            });
        }
    }
}

/// Reads the header and applies the same rules as for downloads.
fn evaluate(candidate: Candidate) -> Result<InstalledModel, String> {
    if NOT_CHAT.is_match(&candidate.name) || candidate.repo.as_deref().is_some_and(|r| NOT_CHAT.is_match(r)) {
        return Err("not a chat model".into());
    }
    let entry = candidate.files.iter().find(|f| quant::is_entry_file(f)).ok_or("no first part")?.clone();
    let info = gguf::read_file(&candidate.dir.join(&entry)).map_err(|e| format!("unreadable GGUF header: {e}"))?;
    if let Some(kind) = info.kind.as_deref().filter(|k| *k != "model") {
        return Err(format!("a {kind} file, not a model"));
    }
    if info.is_draft() || entry.rsplit('/').next().is_some_and(|f| f.to_ascii_lowercase().starts_with("mtp-")) {
        return Err("a speculative-decoding draft, not a model on its own".into());
    }
    if info.ints.contains_key("pooling_type") {
        return Err("an embedding model, not a chat model".into());
    }
    // Unsloth's own layout of looped models (Inkling 2): mainline llama.cpp gives gibberish.
    if info.ints.contains_key("num_loops") {
        return Err("made for Unsloth's llama.cpp build; download it in the app instead".into());
    }
    let label = quant::label(&entry)
        .or_else(|| info.file_type.and_then(quant::label_for_file_type).map(str::to_string))
        .ok_or("unknown quantization")?;
    if !quant::allowed(&label) {
        return Err(format!("{label} is below 4-bit"));
    }
    let context_length = info.context_length();
    if let Some(context) = context_length.filter(|c| *c < MIN_CONTEXT) {
        return Err(format!("{}k context is too short for coding agents", context / 1024));
    }

    let mut files = Vec::new();
    for rel in &candidate.files {
        let size = std::fs::metadata(candidate.dir.join(rel)).map_err(|e| e.to_string())?.len();
        files.push(RemoteFile { path: rel.clone(), size, sha256: None });
    }
    let size = files.iter().map(|f| f.size).sum();
    let installed_at = std::fs::metadata(candidate.dir.join(&entry))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| time::OffsetDateTime::from(t).format(&time::format_description::well_known::Rfc3339).ok())
        .unwrap_or_default();
    let repo = candidate.repo.unwrap_or_default();
    let alias = if repo.is_empty() {
        candidate.name.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' { c } else { '-' }).collect()
    } else {
        installed::alias(&repo)
    };
    Ok(InstalledModel {
        id: candidate.id.unwrap_or_else(|| format!("{repo}:{label}")),
        alias,
        model: CatalogModel {
            repo,
            revision: candidate.revision,
            name: candidate.name,
            description: String::new(),
            capability: None,
            active_params: None,
            arch: info.arch.clone(),
            context_length,
            kv_bytes_per_token: info.kv_bytes_per_token(),
            kv_fixed_bytes: info.kv_cache().map_or(0, |kv| kv.fixed),
            sampling: None,
            reasoning_budget: None,
            quant: QuantGroup { label, files, size },
        },
        installed_at,
        source: candidate.source,
        dir: candidate.dir,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gguf::tests::header_with;

    struct Tree(PathBuf);

    impl Tree {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("superviser-discover-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn file(&self, rel: &str, contents: &[u8]) -> PathBuf {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A chat model header: 32 layers, 8 KV heads, `context` tokens, `file_type`.
    fn model(context: u64, file_type: u64) -> Vec<u8> {
        header_with(
            "qwen3",
            &[("general.file_type", file_type)],
            &[("block_count", 32), ("embedding_length", 4096), ("attention.head_count", 32), ("attention.head_count_kv", 8), ("context_length", context)],
            &[],
        )
    }

    fn names(discovery: &Discovery) -> Vec<String> {
        discovery.models.iter().map(|m| m.id.clone()).collect()
    }

    fn reasons(discovery: &Discovery) -> Vec<String> {
        discovery.skipped.iter().map(|s| format!("{}: {}", s.name, s.reason)).collect()
    }

    #[test]
    fn hugging_face_cache() {
        let tree = Tree::new("hf");
        let snap = "models--unsloth--Qwen3.8-27B-GGUF/snapshots/4af5";
        tree.file(&format!("{snap}/Qwen3.8-27B-UD-Q4_K_XL.gguf"), &model(262_144, 15));
        tree.file(&format!("{snap}/mmproj-F16.gguf"), &model(262_144, 1));
        tree.file(&format!("{snap}/Qwen3.8-27B-Q2_K.gguf"), &model(262_144, 10));
        tree.file(&format!("{snap}/mtp-Qwen3.8-27B.gguf"), &model(262_144, 7));
        tree.file("models--unsloth--Big-GGUF/snapshots/a/Q4_K_M/Big-Q4_K_M-00001-of-00002.gguf", &model(131_072, 15));
        tree.file("models--unsloth--bge-small-en-v1.5-GGUF/snapshots/b/bge-small-en-v1.5-f16.gguf", &model(512, 1));
        let looped = header_with("inkling2", &[], &[("block_count", 48), ("context_length", 1_048_576), ("num_loops", 3)], &[]);
        tree.file("models--unsloth--Inkling-2-Mini-GGUF/snapshots/d/Inkling-2-Mini-UD-Q4_K_M.gguf", &looped);
        tree.file("models--mlx-community--Qwen3.5-2B-4bit/snapshots/c/config.json", b"{}");
        tree.file("models--mlx-community--Qwen3.5-2B-4bit/snapshots/c/model.safetensors", b"");

        let found = scan(&Locations { hf_hub: vec![tree.0.clone()], ..Default::default() });
        assert_eq!(names(&found), ["unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_XL"]);
        let model = &found.models[0];
        assert_eq!(model.source, Source::HuggingFace);
        assert_eq!(model.alias, "qwen3.8-27b");
        assert_eq!(model.model.kv_bytes_per_token, Some(32 * 8 * 256 * 2));
        assert_eq!(model.entry_path().unwrap(), tree.0.join(snap).join("Qwen3.8-27B-UD-Q4_K_XL.gguf"));
        let reasons = reasons(&found);
        assert!(reasons.iter().any(|r| r.contains("Q2_K is below 4-bit")), "{reasons:?}");
        assert!(reasons.iter().any(|r| r.contains("incomplete download (1 of 2 parts)")), "{reasons:?}");
        assert!(reasons.iter().any(|r| r.contains("not a chat model")), "{reasons:?}");
        assert!(reasons.iter().any(|r| r.contains("draft")), "{reasons:?}");
        assert!(reasons.iter().any(|r| r.contains("Unsloth's llama.cpp build")), "{reasons:?}");
        assert_eq!(found.mlx.len(), 1);
    }

    #[test]
    fn lm_studio_and_llama_cpp() {
        let lms = Tree::new("lms");
        lms.file("lmstudio-community/Qwen3-8B-GGUF/Qwen3-8B-Q4_K_M.gguf", &model(40_960, 15));
        lms.file("mlx-community/Qwen3-8B-4bit/config.json", b"{}");
        lms.file("mlx-community/Qwen3-8B-4bit/model.safetensors", b"");
        let cpp = Tree::new("llamacpp");
        cpp.file("unsloth_Qwen3.5-9B-GGUF_Qwen3.5-9B-Q8_0.gguf", &model(262_144, 7));
        cpp.file("ggml-org_tiny_tiny-Q8_0.gguf", &model(8192, 7));

        let found = scan(&Locations { lmstudio: vec![lms.0.clone()], llamacpp: vec![cpp.0.clone()], ..Default::default() });
        assert_eq!(names(&found), ["lmstudio-community/Qwen3-8B-GGUF:Q4_K_M", "llamacpp:unsloth_Qwen3.5-9B-GGUF_Qwen3.5-9B-Q8_0"]);
        assert!(reasons(&found).iter().any(|r| r.contains("8k context is too short")));
        assert_eq!(found.mlx.len(), 1);
    }

    #[test]
    fn ollama_blobs() {
        let tree = Tree::new("ollama");
        let manifest = |digest: &str| {
            serde_json::json!({"layers": [
                {"mediaType": "application/vnd.ollama.image.model", "digest": digest, "size": 1},
                {"mediaType": "application/vnd.ollama.image.template", "digest": "sha256:t", "size": 1},
            ]})
            .to_string()
        };
        tree.file("manifests/registry.ollama.ai/library/qwen3/8b", manifest("sha256:aaa").as_bytes());
        tree.file("manifests/hf.co/unsloth/Qwen3.5-9B-GGUF/Q4_K_M", manifest("sha256:bbb").as_bytes());
        tree.file("manifests/registry.ollama.ai/library/tiny/q2", manifest("sha256:ccc").as_bytes());
        tree.file("blobs/sha256-aaa", &model(40_960, 15));
        tree.file("blobs/sha256-bbb", &model(262_144, 15));
        tree.file("blobs/sha256-ccc", &model(40_960, 10));

        let found = scan(&Locations { ollama: vec![tree.0.clone()], ..Default::default() });
        let mut ids = names(&found);
        ids.sort();
        assert_eq!(ids, ["ollama:hf.co/unsloth/Qwen3.5-9B-GGUF:Q4_K_M", "ollama:qwen3:8b"]);
        let qwen = found.models.iter().find(|m| m.id == "ollama:qwen3:8b").unwrap();
        assert_eq!(qwen.model.quant.label, "Q4_K_M");
        assert_eq!(qwen.alias, "qwen3-8b");
        assert_eq!(qwen.entry_path().unwrap(), tree.0.join("blobs/sha256-aaa"));
        let hf = found.models.iter().find(|m| m.id.starts_with("ollama:hf.co")).unwrap();
        assert_eq!(hf.model.repo, "unsloth/Qwen3.5-9B-GGUF");
        assert!(reasons(&found).iter().any(|r| r.contains("Q2_K is below 4-bit")));
    }

    #[test]
    fn missing_locations_find_nothing() {
        let found = scan(&Locations { hf_hub: vec!["/nonexistent".into()], ..Default::default() });
        assert!(found.models.is_empty() && found.skipped.is_empty());
    }
}
