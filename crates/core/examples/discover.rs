//! Lists the models other apps downloaded on this machine, and whether each fits.

use superviser_core::discover::{Locations, scan};
use superviser_core::hardware::Hardware;

fn main() {
    let locations = Locations::detect();
    println!("{locations:#?}");
    let start = std::time::Instant::now();
    let found = scan(&locations);
    let budget = Hardware::detect().model_budget;
    println!("scanned in {:?}", start.elapsed());
    for m in &found.models {
        let fit = m.model.fit(budget).map_or("does not fit".to_string(), |f| format!("{}k context, ~{:.1} GiB", f.context / 1024, f.required as f64 / (1u64 << 30) as f64));
        println!("  {:<45} {:<11} {:<20} {fit}  ({})", m.id, m.model.quant.label, m.source.label(), m.alias);
    }
    for s in &found.skipped {
        println!("  skipped {} [{}]: {}", s.name, s.source.label(), s.reason);
    }
    for x in &found.mlx {
        println!("  MLX {} [{}]", x.name, x.source.label());
    }
}
