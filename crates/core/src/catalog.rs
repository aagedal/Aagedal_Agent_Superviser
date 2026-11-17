//! The curated model list, and which of its models this machine can run.
//!
//! `catalog/curated.json` is generated from `catalog/sources.json` by
//! `cargo run -p superviser-core --example build_catalog`: it pins each repo to a commit and
//! records the Q4 files, their checksums and the KV-cache size, so filtering by memory needs
//! no network.

use serde::{Deserialize, Serialize};

use crate::hardware::Hardware;
use crate::hub::{Hub, QuantGroup};
use crate::quant;

/// Contexts we try, largest first. Coding agents need at least the smallest: OpenCode's
/// system prompt and tool definitions alone take well over 10k tokens.
pub const CONTEXTS: &[u64] = &[65_536, 32_768];
pub const MIN_CONTEXT: u64 = 32_768;

/// Assumed KV bytes per token when a header can't be read: a dense 8B model without much
/// grouped-query sharing, which overestimates most current models.
const FALLBACK_KV_PER_TOKEN: u64 = 192 * 1024;

const BUNDLED: &str = include_str!("../../../catalog/curated.json");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Catalog {
    pub version: u32,
    pub generated: String,
    pub models: Vec<CatalogModel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogModel {
    pub repo: String,
    /// The commit the files and checksums below come from.
    pub revision: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub arch: String,
    pub context_length: Option<u64>,
    pub kv_bytes_per_token: Option<u64>,
    /// KV cache that doesn't grow with context (sliding-window layers).
    #[serde(default)]
    pub kv_fixed_bytes: u64,
    pub quant: QuantGroup,
}

/// How a model would run here.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct Fit {
    pub context: u64,
    /// Weights, KV cache at `context` and runtime buffers.
    pub required: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Offer {
    #[serde(flatten)]
    pub model: CatalogModel,
    pub fit: Fit,
    /// The model we'd suggest first: the largest one that fits with the full context.
    pub recommended: bool,
}

impl Catalog {
    pub fn bundled() -> Self {
        serde_json::from_str(BUNDLED).expect("bundled catalog is valid")
    }

    /// The models that fit `hardware`: the recommended one, then the rest largest first.
    /// Models that don't fit are left out.
    pub fn offers(&self, hardware: &Hardware) -> Vec<Offer> {
        let mut offers: Vec<Offer> = self
            .models
            .iter()
            .filter_map(|model| {
                let fit = model.fit(hardware.model_budget)?;
                Some(Offer { model: model.clone(), fit, recommended: false })
            })
            .collect();
        offers.sort_by_key(|offer| std::cmp::Reverse(offer.model.quant.size));
        // A model squeezed down to the smallest context leaves no headroom: suggest the
        // largest one that gets the full context, if any does.
        let full = CONTEXTS[0];
        let pick = offers.iter().position(|o| o.fit.context >= full || o.model.context_length.is_some_and(|c| c <= o.fit.context));
        if let Some(index) = pick.or((!offers.is_empty()).then_some(0)) {
            let mut offer = offers.remove(index);
            offer.recommended = true;
            offers.insert(0, offer);
        }
        offers
    }
}

impl CatalogModel {
    pub fn fit(&self, budget: u64) -> Option<Fit> {
        fit(self.quant.size + self.kv_fixed_bytes, self.kv_bytes_per_token, self.context_length, budget)
    }

    /// Resolves a repo into a catalog entry: its default 4-bit quant, pinned to the current
    /// commit, with the KV-cache size from that file's header.
    pub async fn resolve(hub: &Hub, repo: &str, name: Option<&str>, description: &str) -> anyhow::Result<Self> {
        let info = hub.model_info(repo).await?;
        let groups = hub.quant_groups(repo, &info.sha).await?;
        let label = quant::preferred(groups.iter().map(|g| g.label.as_str()))
            .ok_or_else(|| anyhow::anyhow!("{repo} has no 4-bit GGUF files"))?
            .to_string();
        let group = groups.into_iter().find(|g| g.label == label).unwrap();
        let entry = group.entry_file().ok_or_else(|| anyhow::anyhow!("{repo}: no first shard for {label}"))?;
        let header = hub.gguf_header(repo, &info.sha, &entry.path).await;
        let (arch, context_length, kv) = match &header {
            Ok(h) => (h.arch.clone(), h.context_length(), h.kv_cache()),
            Err(e) => {
                tracing::warn!("{repo}: {e:#}");
                (String::new(), None, None)
            }
        };
        Ok(Self {
            repo: repo.to_string(),
            revision: info.sha,
            name: name.map(str::to_string).unwrap_or_else(|| display_name(repo)),
            description: description.to_string(),
            arch,
            context_length,
            kv_bytes_per_token: kv.map(|kv| kv.per_token),
            kv_fixed_bytes: kv.map_or(0, |kv| kv.fixed),
            quant: group,
        })
    }
}

/// The largest of [`CONTEXTS`] (capped at the model's own) at which the model fits `budget`.
/// `weights` includes any context-independent cache.
pub fn fit(weights: u64, kv_per_token: Option<u64>, model_context: Option<u64>, budget: u64) -> Option<Fit> {
    let kv = kv_per_token.unwrap_or(FALLBACK_KV_PER_TOKEN);
    // Compute buffers grow with the model; half a GiB covers the small ones.
    let overhead = (512 << 20) + weights / 32;
    let max_context = model_context.unwrap_or(MIN_CONTEXT);
    if max_context < MIN_CONTEXT {
        return None;
    }
    CONTEXTS.iter().map(|&c| c.min(max_context)).find_map(|context| {
        let required = weights + kv * context + overhead;
        (required <= budget).then_some(Fit { context, required })
    })
}

/// `unsloth/Qwen3.5-9B-GGUF` → `Qwen3.5 9B`.
pub fn display_name(repo: &str) -> String {
    let name = repo.rsplit('/').next().unwrap_or(repo);
    let name = name.strip_suffix("-GGUF").or_else(|| name.strip_suffix("-gguf")).unwrap_or(name);
    name.replace('-', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    #[test]
    fn picks_the_largest_context_that_fits() {
        // 5 GiB of weights, 32 KiB per token: 64k context needs 2 GiB of cache.
        let fit64 = fit(5 * GIB, Some(32 * 1024), Some(262_144), 10 * GIB).unwrap();
        assert_eq!(fit64.context, 65_536);
        let fit32 = fit(5 * GIB, Some(32 * 1024), Some(262_144), 7 * GIB).unwrap();
        assert_eq!(fit32.context, 32_768);
        assert_eq!(fit(5 * GIB, Some(32 * 1024), Some(262_144), 6 * GIB), None);
    }

    #[test]
    fn short_context_models_are_not_offered() {
        assert_eq!(fit(GIB, Some(1024), Some(8192), 64 * GIB), None);
    }

    #[test]
    fn bundled_catalog_only_holds_4_bit_or_better() {
        let catalog = Catalog::bundled();
        assert!(!catalog.models.is_empty());
        for model in &catalog.models {
            assert!(quant::allowed(&model.quant.label), "{} uses {}", model.repo, model.quant.label);
            assert!(model.quant.entry_file().is_some(), "{} has no entry file", model.repo);
            assert_eq!(model.revision.len(), 40, "{} is not pinned to a commit", model.repo);
        }
    }

    #[test]
    fn offers_leave_out_what_does_not_fit() {
        let catalog = Catalog::bundled();
        let small = Hardware { total_memory: 16 * GIB, model_budget: 10 * GIB, unified_memory: true, os: "macos", arch: "aarch64" };
        let offers = catalog.offers(&small);
        assert!(offers.iter().all(|o| o.fit.required <= small.model_budget));
        assert!(offers.len() < catalog.models.len());
        assert_eq!(offers.iter().filter(|o| o.recommended).count(), usize::from(!offers.is_empty()));
        assert!(offers.first().is_none_or(|o| o.recommended));
    }

    #[test]
    fn names() {
        assert_eq!(display_name("unsloth/Qwen3.5-9B-GGUF"), "Qwen3.5 9B");
    }
}
