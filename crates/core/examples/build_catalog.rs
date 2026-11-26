//! Regenerates `catalog/curated.json` from `catalog/sources.json`. With `--judge-only`, only
//! the judge's entry is refreshed and the chat models keep their pinned commits. With
//! `--new-only`, models already in `curated.json` keep their pins (their names, descriptions
//! and ratings still come from `sources.json`) and only new ones are fetched.

use serde::Deserialize;
use superviser_core::catalog::{Catalog, CatalogModel, Sampling};
use superviser_core::hub::Hub;

#[derive(Deserialize)]
struct Sources {
    models: Vec<Source>,
    judge: Option<Source>,
}

#[derive(Deserialize)]
struct Source {
    repo: String,
    name: Option<String>,
    #[serde(default)]
    description: String,
    capability: Option<u8>,
    active_params: Option<f32>,
    sampling: Option<Sampling>,
    reasoning_budget: Option<u32>,
    spec_draft_n_max: Option<u32>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../catalog");
    let sources: Sources = serde_json::from_str(&std::fs::read_to_string(root.join("sources.json"))?)?;
    let hub = Hub::new(std::env::var("HF_TOKEN").ok());
    let new_only = std::env::args().any(|a| a == "--new-only");
    let pinned = Catalog::bundled();
    let judge = match &sources.judge {
        Some(_) if new_only => pinned.judge.clone(),
        Some(source) => Some(CatalogModel::resolve(&hub, &source.repo, source.name.as_deref(), &source.description).await?),
        None => None,
    };
    if std::env::args().any(|a| a == "--judge-only") {
        let mut catalog = Catalog::bundled();
        catalog.judge = judge;
        std::fs::write(root.join("curated.json"), serde_json::to_string_pretty(&catalog)? + "\n")?;
        return Ok(());
    }
    let mut models = Vec::new();
    for source in sources.models {
        let mut model = match pinned.models.iter().find(|m| new_only && m.repo == source.repo) {
            Some(model) => model.clone(),
            None => CatalogModel::resolve(&hub, &source.repo, source.name.as_deref(), &source.description).await?,
        };
        if let Some(name) = &source.name {
            model.name = name.clone();
        }
        model.description = source.description.clone();
        model.capability = source.capability;
        model.active_params = source.active_params;
        model.sampling = source.sampling;
        model.reasoning_budget = source.reasoning_budget;
        model.spec_draft_n_max = source.spec_draft_n_max;
        eprintln!(
            "{:<50} {:<11} {:>6.1} GiB  kv {:>7} B/token  ctx {:?}  {}",
            model.repo,
            model.quant.label,
            model.quant.size as f64 / (1u64 << 30) as f64,
            model.kv_bytes_per_token.map_or("?".into(), |v| v.to_string()),
            model.context_length,
            model.arch,
        );
        models.push(model);
    }
    let generated = if new_only && models.len() == pinned.models.len() { pinned.generated } else { time::OffsetDateTime::now_utc().date().to_string() };
    let catalog = Catalog { version: 1, generated, models, judge };
    std::fs::write(root.join("curated.json"), serde_json::to_string_pretty(&catalog)? + "\n")?;
    Ok(())
}
