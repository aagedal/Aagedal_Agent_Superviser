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

/// What a conversation saved to RAM takes per token beyond its KV cells (measured: 7.4 KiB).
const SAVED_BYTES_PER_TOKEN: u64 = 8 * 1024;
/// Room for a short conversation saved beside it, such as OpenCode's title requests to the
/// same model (a short one measured 150 MiB).
const SAVED_EXTRA_BYTES: u64 = 256 << 20;

/// Tokens the judge reads at most. A decision model reads its whole input in one batch, and
/// the judge's input is cut to about 6,000 characters, so this leaves room for dense text.
pub const JUDGE_CONTEXT: u64 = 8192;
/// What the judge takes beside its weights at that context: KV cache and compute buffers
/// (measured for d3-mini: 2.1 GiB).
const JUDGE_BUFFERS: u64 = 2304 << 20;

const BUNDLED: &str = include_str!("../../../catalog/curated.json");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Catalog {
    pub version: u32,
    pub generated: String,
    pub models: Vec<CatalogModel>,
    /// The decision model that judges suspicious calls, when the judge is on.
    #[serde(default)]
    pub judge: Option<CatalogModel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogModel {
    pub repo: String,
    /// The commit the files and checksums below come from.
    pub revision: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// How well it does agentic coding, 1 to 10: our judgment, set in `sources.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<u8>,
    /// Billions of parameters read per token. Generating is memory-bound, so this sets speed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_params: Option<f32>,
    pub arch: String,
    pub context_length: Option<u64>,
    pub kv_bytes_per_token: Option<u64>,
    /// KV cache that doesn't grow with context (sliding-window layers).
    #[serde(default)]
    pub kv_fixed_bytes: u64,
    /// The model card's recommended sampling, set in `sources.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling: Option<Sampling>,
    /// Most tokens it may think for before it's told to answer, for models whose card warns
    /// they can get stuck in reasoning loops. Set in `sources.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_budget: Option<u32>,
    /// Optional override for `--spec-draft-n-max` when neural speculative decoding (MTP / DSpark)
    /// is active. Defaults to Unsloth's recommended `2`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec_draft_n_max: Option<u32>,
    pub quant: QuantGroup,
}

/// Sampling a model card recommends, which llama-server uses unless a request says otherwise.
/// What a card doesn't mention is off, as in the reference implementations; llama.cpp's own
/// defaults would add top-k 40 and min-p 0.05.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Sampling {
    pub temperature: f32,
    #[serde(default = "one")]
    pub top_p: f32,
    /// 0 is off.
    #[serde(default)]
    pub top_k: u32,
    #[serde(default)]
    pub min_p: f32,
    #[serde(default)]
    pub presence_penalty: f32,
    #[serde(default = "one")]
    pub repeat_penalty: f32,
}

fn one() -> f32 {
    1.0
}

/// How llama-server stores the KV cache. 8-bit (q8_0: 32 values in 34 bytes) takes 17/32 of
/// 16-bit's memory, so models get the full context on smaller Macs and more fit at once. It
/// reads prompts a little slower (7% on Gemma 3 4B at 24k tokens on an M5 Pro), and its error
/// is far below the 4-bit weights'. The GGUF header gives the 16-bit size.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KvType {
    #[default]
    Q8,
    F16,
}

impl KvType {
    /// What `f16` bytes of cache take in this type.
    pub fn bytes(self, f16: u64) -> u64 {
        match self {
            Self::Q8 => ((u128::from(f16) * 17) / 32).min(u128::from(u64::MAX)) as u64,
            Self::F16 => f16,
        }
    }

    /// llama-server's name for it (`--cache-type-k`, `--cache-type-v`).
    pub fn llama_name(self) -> &'static str {
        match self {
            Self::Q8 => "q8_0",
            Self::F16 => "f16",
        }
    }
}

/// How a model would run here.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct Fit {
    pub context: u64,
    /// Weights, KV cache at `context` and runtime buffers.
    pub required: u64,
    pub kv: KvType,
}

#[derive(Debug, Clone, Serialize)]
pub struct Offer {
    #[serde(flatten)]
    pub model: CatalogModel,
    pub fit: Fit,
    /// The model we'd suggest first: the largest one that fits with the full context.
    pub recommended: bool,
    /// Few enough active parameters to answer quickly; see [`CatalogModel::fast`].
    pub fast: bool,
}

/// What a model in the short list is for.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The most capable model that fits.
    MostCapable,
    /// The most capable fast one, when the most capable is slow.
    Fast,
    /// The most capable one that leaves most of the memory to other apps.
    Light,
    /// The smallest, for basic tasks.
    Basic,
}

#[derive(Debug, Clone, Serialize)]
pub struct Pick {
    pub role: Role,
    #[serde(flatten)]
    pub offer: Offer,
}

/// Models using at most this share of total memory count as leaving most of it to other apps.
const LIGHT_SHARE: u64 = 3;
/// ...and never more than this, so a lighter pick stays a 12-20B model on big machines too
/// (gpt-oss 20B needs 13.2 GiB at 64k context).
const LIGHT_MAX: u64 = 16 << 30;
/// Models reading at most this many billion parameters per token count as fast.
const FAST_ACTIVE_PARAMS: f32 = 12.0;

impl Catalog {
    pub fn bundled() -> Self {
        serde_json::from_str(BUNDLED).expect("bundled catalog is valid")
    }

    /// The curated entry for `repo`, in any quant, for how to run it (sampling, reasoning
    /// budget). Looked up here rather than in a model's manifest, so models installed earlier or
    /// found in other apps get current values.
    pub fn curated(&self, repo: &str) -> Option<&CatalogModel> {
        self.models.iter().find(|m| m.repo.eq_ignore_ascii_case(repo))
    }

    /// The models that fit `hardware`: the recommended one, then the rest largest first.
    /// Models that don't fit are left out.
    pub fn offers(&self, hardware: &Hardware) -> Vec<Offer> {
        let mut offers: Vec<Offer> = self
            .models
            .iter()
            .filter_map(|model| {
                let fit = model.fit(hardware.model_budget)?;
                Some(Offer { model: model.clone(), fit, recommended: false, fast: model.fast() })
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

    /// Up to four of `offers` for people who'd rather not compare models, in the order of
    /// [`Role`]. Each model appears once, and a role with nothing distinct to offer is left
    /// out, so small machines get fewer choices.
    pub fn picks(offers: &[Offer], total_memory: u64) -> Vec<Pick> {
        // Most capable first; on a tie, the faster, then the smaller one.
        let best = |candidates: &mut dyn Iterator<Item = &Offer>| {
            candidates
                .max_by(|a, b| {
                    let rank = |o: &Offer| (o.model.capability.unwrap_or(0), o.fast, std::cmp::Reverse(o.fit.required));
                    rank(a).cmp(&rank(b))
                })
                .cloned()
        };
        let mut picks: Vec<Pick> = Vec::new();
        let unpicked = |picks: &[Pick], o: &Offer| picks.iter().all(|p| p.offer.model.repo != o.model.repo);

        let Some(top) = best(&mut offers.iter()) else { return picks };
        let top_fast = top.fast;
        let top_required = top.fit.required;
        picks.push(Pick { role: Role::MostCapable, offer: top });
        if !top_fast
            && let Some(offer) = best(&mut offers.iter().filter(|o| o.fast && unpicked(&picks, o)))
        {
            picks.push(Pick { role: Role::Fast, offer });
        }
        let light = |o: &&Offer| o.fit.required <= (total_memory / LIGHT_SHARE).min(LIGHT_MAX) && o.fit.required < top_required;
        if let Some(offer) = best(&mut offers.iter().filter(light).filter(|o| unpicked(&picks, o))) {
            picks.push(Pick { role: Role::Light, offer });
        }
        let smallest_picked = picks.iter().map(|p| p.offer.fit.required).min().unwrap_or(u64::MAX);
        if let Some(offer) = offers.iter().filter(|o| unpicked(&picks, o) && o.fit.required < smallest_picked).min_by_key(|o| o.fit.required) {
            picks.push(Pick { role: Role::Basic, offer: offer.clone() });
        }
        picks
    }
}

impl CatalogModel {
    pub fn fast(&self) -> bool {
        self.active_params.is_some_and(|b| b <= FAST_ACTIVE_PARAMS)
    }

    /// How it fits `budget` with the default (8-bit) KV cache.
    pub fn fit(&self, budget: u64) -> Option<Fit> {
        self.fit_with(budget, KvType::default())
    }

    pub fn fit_with(&self, budget: u64, kv: KvType) -> Option<Fit> {
        fit(self.quant.size.saturating_add(kv.bytes(self.kv_fixed_bytes)), self.kv_bytes_per_token, self.context_length, budget, kv)
    }

    /// About what a conversation of `tokens` takes saved to disk: its KV cells and the
    /// sliding-window layers' cache.
    pub fn saved_bytes(&self, tokens: u64, kv: KvType) -> u64 {
        let variable = self.kv_bytes_per_token.unwrap_or(FALLBACK_KV_PER_TOKEN).saturating_mul(tokens);
        kv.bytes(variable.saturating_add(self.kv_fixed_bytes))
    }

    /// Memory the judge takes while it runs beside a model.
    pub fn judge_required(&self) -> u64 {
        self.quant.size.saturating_add(JUDGE_BUFFERS)
    }

    /// RAM llama-server may keep saved conversations in: enough for one at full context, so
    /// the agent's survives a short request in between, but never more than the model leaves
    /// free of `ram_budget`. With less, a long conversation is read again after one.
    pub fn prompt_cache(&self, fit: &Fit, ram_budget: u64) -> u64 {
        let per_token = fit.kv.bytes(self.kv_bytes_per_token.unwrap_or(FALLBACK_KV_PER_TOKEN)).saturating_add(SAVED_BYTES_PER_TOKEN);
        let conversation = per_token
            .saturating_mul(fit.context)
            .saturating_add(fit.kv.bytes(self.kv_fixed_bytes))
            .saturating_add(SAVED_EXTRA_BYTES);
        conversation.min(ram_budget.saturating_sub(fit.required))
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
            capability: None,
            active_params: None,
            arch,
            context_length,
            kv_bytes_per_token: kv.map(|kv| kv.per_token),
            kv_fixed_bytes: kv.map_or(0, |kv| kv.fixed),
            sampling: None,
            reasoning_budget: None,
            spec_draft_n_max: None,
            quant: group,
        })
    }
}

/// The largest of [`CONTEXTS`] (capped at the model's own) at which the model fits `budget`.
/// `weights` includes any context-independent cache; `kv_per_token` is the 16-bit size.
pub fn fit(weights: u64, kv_per_token: Option<u64>, model_context: Option<u64>, budget: u64, kv_type: KvType) -> Option<Fit> {
    let kv = kv_type.bytes(kv_per_token.unwrap_or(FALLBACK_KV_PER_TOKEN));
    // Compute buffers grow with the model; half a GiB covers the small ones.
    let overhead = (512u64 << 20).saturating_add(weights / 32);
    let max_context = model_context.unwrap_or(MIN_CONTEXT);
    if max_context < MIN_CONTEXT {
        return None;
    }
    CONTEXTS.iter().map(|&c| c.min(max_context)).find_map(|context| {
        let required = weights.checked_add(kv.checked_mul(context)?)?.checked_add(overhead)?;
        (required <= budget).then_some(Fit { context, required, kv: kv_type })
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
    use crate::hardware;

    const GIB: u64 = 1 << 30;

    #[test]
    fn picks_the_largest_context_that_fits() {
        // 5 GiB of weights, 32 KiB per token: 64k context needs 2 GiB of cache.
        let fit64 = fit(5 * GIB, Some(32 * 1024), Some(262_144), 10 * GIB, KvType::F16).unwrap();
        assert_eq!(fit64.context, 65_536);
        let fit32 = fit(5 * GIB, Some(32 * 1024), Some(262_144), 7 * GIB, KvType::F16).unwrap();
        assert_eq!(fit32.context, 32_768);
        assert_eq!(fit(5 * GIB, Some(32 * 1024), Some(262_144), 6 * GIB, KvType::F16), None);
    }

    #[test]
    fn an_8_bit_cache_gets_more_context_in_the_same_memory() {
        // 64k context at 17 KiB per token is 1.06 GiB of cache instead of 2.
        let fit = fit(5 * GIB, Some(32 * 1024), Some(262_144), 7 * GIB, KvType::Q8).unwrap();
        assert_eq!((fit.context, fit.kv), (65_536, KvType::Q8));
        assert_eq!(KvType::Q8.bytes(32 * 1024), 17 * 1024);
        assert_eq!(KvType::default(), KvType::Q8);
    }

    #[test]
    fn short_context_models_are_not_offered() {
        assert_eq!(fit(GIB, Some(1024), Some(8192), 64 * GIB, KvType::Q8), None);
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
        let small = Hardware { total_memory: 16 * GIB, model_budget: 10 * GIB, ram_budget: 12 * GIB, unified_memory: true, os: "macos", arch: "aarch64" };
        let offers = catalog.offers(&small);
        assert!(offers.iter().all(|o| o.fit.required <= small.model_budget));
        assert!(offers.len() < catalog.models.len());
        assert_eq!(offers.iter().filter(|o| o.recommended).count(), usize::from(!offers.is_empty()));
        assert!(offers.first().is_none_or(|o| o.recommended));
    }

    #[test]
    fn picks_span_capability_speed_and_memory() {
        let catalog = Catalog::bundled();
        let names = |total: u64| {
            let hardware = Hardware { total_memory: total, model_budget: hardware::model_budget(total, true, None), ram_budget: hardware::ram_budget(total, true), unified_memory: true, os: "macos", arch: "aarch64" };
            Catalog::picks(&catalog.offers(&hardware), total).into_iter().map(|p| (p.role, p.offer.model.name)).collect::<Vec<_>>()
        };
        assert_eq!(
            names(64 * GIB),
            [(Role::MostCapable, "Qwen3.8 27B".into()), (Role::Fast, "Inkling 2 Mini".into()), (Role::Light, "Gemma 4 12B".into()), (Role::Basic, "Qwen3.5 4B".into())]
        );
        // Few models fit, and the most capable is already fast.
        assert_eq!(names(16 * GIB), [(Role::MostCapable, "Gemma 4 12B".into()), (Role::Light, "Qwen3.5 4B".into())]);
        assert!(names(4 * GIB).is_empty());
    }

    #[test]
    fn every_curated_model_is_rated() {
        for model in &Catalog::bundled().models {
            assert!(model.capability.is_some_and(|c| (1..=10).contains(&c)), "{} has no capability", model.repo);
            assert!(model.active_params.is_some(), "{} has no active_params", model.repo);
            assert!(model.sampling.is_some(), "{} has no recommended sampling", model.repo);
        }
    }

    #[test]
    fn sampling_leaves_unmentioned_samplers_off() {
        let sampling: Sampling = serde_json::from_str(r#"{ "temperature": 0.15 }"#).unwrap();
        assert_eq!(sampling, Sampling { temperature: 0.15, top_p: 1.0, top_k: 0, min_p: 0.0, presence_penalty: 0.0, repeat_penalty: 1.0 });
        let catalog = Catalog::bundled();
        let inkling = catalog.curated("unsloth/inkling-2-mini-gguf").unwrap();
        assert_eq!(inkling.sampling.map(|s| (s.temperature, s.top_k)), Some((1.0, 0)));
        assert!(inkling.reasoning_budget.is_some());
        assert!(catalog.curated("someone/else-GGUF").is_none());
    }

    #[test]
    fn prompt_cache_holds_one_conversation_and_stays_in_free_memory() {
        let catalog = Catalog::bundled();
        let model = catalog.models.iter().find(|m| m.repo == "unsloth/Qwen3.5-4B-GGUF").unwrap();
        let fit = Fit { context: 32_768, required: 5 * GIB, kv: KvType::F16 };
        // 40 KiB per token at 32k context, plus room for a short one.
        assert_eq!(model.prompt_cache(&fit, 60 * GIB), (1280 + 256) << 20);
        // 8-bit: 17 + 8 KiB per token.
        assert_eq!(model.prompt_cache(&Fit { kv: KvType::Q8, ..fit }, 60 * GIB), (800 + 256) << 20);
        assert_eq!(model.prompt_cache(&fit, 5 * GIB + (500 << 20)), 500 << 20);
        assert_eq!(model.prompt_cache(&fit, 5 * GIB), 0);
        assert_eq!(model.prompt_cache(&fit, 4 * GIB), 0);
    }

    #[test]
    fn bundled_judge_is_pinned_and_small() {
        let judge = Catalog::bundled().judge.expect("a judge model");
        assert_eq!(judge.revision.len(), 40);
        assert!(judge.quant.entry_file().is_some());
        assert!(judge.judge_required() < 6 * GIB, "{}", judge.judge_required());
    }

    #[test]
    fn names() {
        assert_eq!(display_name("unsloth/Qwen3.5-9B-GGUF"), "Qwen3.5 9B");
    }
}
