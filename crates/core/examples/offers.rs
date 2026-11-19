//! Prints which curated models each memory size would be offered.

use superviser_core::catalog::Catalog;
use superviser_core::hardware::{Hardware, model_budget, ram_budget};

fn main() {
    const GIB: u64 = 1 << 30;
    let catalog = Catalog::bundled();
    for total_gb in [8, 16, 24, 32, 48, 64, 96, 128] {
        let total = total_gb * GIB;
        let hardware = Hardware { total_memory: total, model_budget: model_budget(total, true, None), ram_budget: ram_budget(total, true), unified_memory: true, os: "macos", arch: "aarch64" };
        let offers = catalog.offers(&hardware);
        let names: Vec<String> = offers
            .iter()
            .map(|o| {
                let cache = o.model.prompt_cache(&o.fit, hardware.ram_budget) as f64 / GIB as f64;
                format!("{}{} ({}k, {:.1} GiB + {cache:.1} cache)", if o.recommended { "*" } else { "" }, o.model.name, o.fit.context / 1024, o.fit.required as f64 / GIB as f64)
            })
            .collect();
        println!("{total_gb:>3} GB Mac (budget {:.1} GiB): {}", hardware.model_budget as f64 / GIB as f64, if names.is_empty() { "nothing fits".into() } else { names.join(", ") });
    }
}
