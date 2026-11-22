//! The parts of the Hugging Face Hub API we use: search, file listings, GGUF headers and
//! verified, resumable downloads.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, bail};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::gguf::{self, GgufError, GgufInfo};
use crate::quant;

pub const ENDPOINT: &str = "https://huggingface.co";

/// First header read; doubled until the architecture keys fit, up to `HEADER_MAX`.
const HEADER_FIRST: usize = 1 << 20;
const HEADER_MAX: usize = 64 << 20;

#[derive(Clone)]
pub struct Hub {
    client: reqwest::Client,
    endpoint: String,
    token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RemoteFile {
    pub path: String,
    pub size: u64,
    /// SHA-256 of the content (the LFS object id); downloads are checked against it.
    pub sha256: Option<String>,
}

/// The files of one quantization, possibly split into shards.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QuantGroup {
    pub label: String,
    pub files: Vec<RemoteFile>,
    pub size: u64,
}

impl QuantGroup {
    /// The file to load: the only one, or the first shard.
    pub fn entry_file(&self) -> Option<&RemoteFile> {
        self.files.iter().find(|f| quant::is_entry_file(&f.path))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub id: String,
    #[serde(default)]
    pub downloads: u64,
    #[serde(default)]
    pub likes: u64,
    #[serde(default, rename = "createdAt")]
    pub created_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TreeEntry {
    #[serde(rename = "type")]
    kind: String,
    path: String,
    #[serde(default)]
    size: u64,
    lfs: Option<Lfs>,
}

#[derive(Debug, Deserialize)]
struct Lfs {
    oid: String,
    size: u64,
}

#[derive(Debug, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub sha: String,
    #[serde(default)]
    pub gated: serde_json::Value,
}

impl Hub {
    pub fn new(token: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(concat!("agent-superviser/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("HTTP client");
        Self { client, endpoint: ENDPOINT.to_string(), token: token.filter(|t| !t.is_empty()) }
    }

    fn get(&self, url: &str) -> reqwest::RequestBuilder {
        let request = self.client.get(url);
        match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    pub async fn model_info(&self, repo: &str) -> anyhow::Result<ModelInfo> {
        let url = format!("{}/api/models/{repo}", self.endpoint);
        let response = self.get(&url).send().await?.error_for_status().with_context(|| format!("model {repo}"))?;
        Ok(response.json().await?)
    }

    /// GGUF repos matching `query`, most downloaded first.
    pub async fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<SearchHit>> {
        let url = format!("{}/api/models", self.endpoint);
        let limit = limit.to_string();
        let params = [("search", query), ("filter", "gguf"), ("sort", "downloads"), ("limit", &limit)];
        let response = self.get(&url).query(&params).send().await?.error_for_status()?;
        Ok(response.json().await?)
    }

    /// The quantizations a repo offers at `revision`, without those under 4 bits.
    pub async fn quant_groups(&self, repo: &str, revision: &str) -> anyhow::Result<Vec<QuantGroup>> {
        let url = format!("{}/api/models/{repo}/tree/{revision}?recursive=true", self.endpoint);
        let entries: Vec<TreeEntry> = self.get(&url).send().await?.error_for_status()?.json().await?;
        Ok(group_quants(entries))
    }

    pub fn file_url(&self, repo: &str, revision: &str, path: &str) -> String {
        format!("{}/{repo}/resolve/{revision}/{path}", self.endpoint)
    }

    /// The metadata at the start of a GGUF file, read with range requests.
    pub async fn gguf_header(&self, repo: &str, revision: &str, path: &str) -> anyhow::Result<GgufInfo> {
        let url = self.file_url(repo, revision, path);
        let mut want = HEADER_FIRST;
        loop {
            let response = self
                .get(&url)
                .header(reqwest::header::RANGE, format!("bytes=0-{}", want - 1))
                .send()
                .await?
                .error_for_status()?;
            let bytes = response.bytes().await?;
            match gguf::parse(&bytes) {
                Err(GgufError::NeedMore) if bytes.len() >= want && want < HEADER_MAX => want *= 4,
                result => return result.with_context(|| format!("reading the header of {repo}/{path}")),
            }
        }
    }

    /// Downloads `file` to `dest`, resuming a `.part` file left by an earlier attempt and
    /// checking the SHA-256. `progress` gets the bytes of this file written so far.
    pub async fn download(
        &self,
        repo: &str,
        revision: &str,
        file: &RemoteFile,
        dest: &Path,
        cancel: &Arc<AtomicBool>,
        mut progress: impl FnMut(u64),
    ) -> anyhow::Result<()> {
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let part = part_path(dest);
        let mut hasher = Sha256::new();
        let mut have = 0u64;
        if let Ok(mut existing) = tokio::fs::File::open(&part).await {
            let mut buf = vec![0u8; 1 << 20];
            loop {
                let n = existing.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                have += n as u64;
            }
        }
        if have > file.size {
            tokio::fs::remove_file(&part).await?;
            hasher = Sha256::new();
            have = 0;
        }
        progress(have);

        if have < file.size {
            let url = self.file_url(repo, revision, &file.path);
            let mut request = self.get(&url);
            if have > 0 {
                request = request.header(reqwest::header::RANGE, format!("bytes={have}-"));
            }
            let response = request.send().await?.error_for_status()?;
            if have > 0 && response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
                // The server ignored the range: start over.
                hasher = Sha256::new();
                have = 0;
            }
            let mut out = tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .append(have > 0)
                .truncate(have == 0)
                .open(&part)
                .await?;
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                if cancel.load(Ordering::Relaxed) {
                    out.flush().await?;
                    bail!("download cancelled");
                }
                let chunk = chunk?;
                hasher.update(&chunk);
                out.write_all(&chunk).await?;
                have += chunk.len() as u64;
                progress(have);
            }
            out.flush().await?;
        }

        if have != file.size {
            bail!("{} ended after {have} of {} bytes", file.path, file.size);
        }
        if let Some(expected) = &file.sha256 {
            let actual = hex::encode(hasher.finalize());
            if !actual.eq_ignore_ascii_case(expected) {
                tokio::fs::remove_file(&part).await.ok();
                bail!("{} failed its checksum (got {actual}, expected {expected})", file.path);
            }
        }
        tokio::fs::rename(&part, dest).await?;
        Ok(())
    }
}

/// Where [`Hub::download`] keeps a download to `dest` until it is complete.
pub fn part_path(dest: &Path) -> std::path::PathBuf {
    dest.with_extension("gguf.part")
}

fn group_quants(entries: Vec<TreeEntry>) -> Vec<QuantGroup> {
    // A repo with files in mainline llama.cpp's layout publishes its plain GGUFs for another build.
    let mainline = entries.iter().any(|e| e.kind == "file" && quant::is_mainline(&e.path));
    let mut groups: Vec<QuantGroup> = Vec::new();
    for entry in entries {
        let gguf = if mainline { quant::is_mainline(&entry.path) } else { entry.path.to_ascii_lowercase().ends_with(".gguf") };
        if entry.kind != "file" || !gguf || quant::is_auxiliary(&entry.path) {
            continue;
        }
        let Some(label) = quant::label(&entry.path) else { continue };
        if !quant::allowed(&label) {
            continue;
        }
        let (size, sha256) = match entry.lfs {
            Some(lfs) => (lfs.size, Some(lfs.oid)),
            None => (entry.size, None),
        };
        let file = RemoteFile { path: entry.path, size, sha256 };
        match groups.iter_mut().find(|g| g.label == label) {
            Some(group) => {
                group.size += file.size;
                group.files.push(file);
            }
            None => groups.push(QuantGroup { label, size: file.size, files: vec![file] }),
        }
    }
    for group in &mut groups {
        group.files.sort_by(|a, b| a.path.cmp(&b.path));
    }
    groups.sort_by_key(|g| g.size);
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, size: u64) -> TreeEntry {
        TreeEntry { kind: "file".into(), path: path.into(), size: 100, lfs: Some(Lfs { oid: format!("sha-{path}"), size }) }
    }

    #[test]
    fn groups_shards_and_drops_low_bit_and_aux_files() {
        let groups = group_quants(vec![
            entry("Q4_K_M/m-Q4_K_M-00002-of-00002.gguf", 5),
            entry("Q4_K_M/m-Q4_K_M-00001-of-00002.gguf", 10),
            entry("m-Q2_K.gguf", 3),
            entry("m-UD-IQ1_S.gguf", 2),
            entry("m-Q8_0.gguf", 30),
            entry("mmproj-F16.gguf", 1),
            TreeEntry { kind: "file".into(), path: "README.md".into(), size: 1, lfs: None },
        ]);
        let labels: Vec<&str> = groups.iter().map(|g| g.label.as_str()).collect();
        assert_eq!(labels, ["Q4_K_M", "Q8_0"]);
        assert_eq!(groups[0].size, 15);
        assert_eq!(groups[0].entry_file().unwrap().path, "Q4_K_M/m-Q4_K_M-00001-of-00002.gguf");
    }

    #[test]
    fn prefers_files_for_mainline_llama_cpp() {
        let groups = group_quants(vec![
            entry("m-UD-Q4_K_M.gguf", 10),
            entry("m-UD-Q8_K_XL.gguf", 20),
            entry("Mainline_Support/m-UD-Q4_K_M.gguf_file", 11),
            entry("mmproj-F16.gguf", 1),
        ]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].label, "UD-Q4_K_M");
        assert_eq!(groups[0].entry_file().unwrap().path, "Mainline_Support/m-UD-Q4_K_M.gguf_file");
    }
}
