//! Times the scanner over the parity corpus: `cargo run --release -p superviser-scan --example bench`.

fn main() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/parity/data/corpus.jsonl");
    let rows: Vec<serde_json::Value> = std::fs::read_to_string(path).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let scanner = superviser_scan::Scanner::new();
    let mut times: Vec<std::time::Duration> = Vec::new();
    for row in &rows {
        let pending = [(row["name"].as_str().unwrap().to_string(), row["arguments"].as_str().unwrap().to_string())];
        let start = std::time::Instant::now();
        std::hint::black_box(scanner.signals(&row["request"], &pending));
        times.push(start.elapsed());
    }
    times.sort();
    let total: std::time::Duration = times.iter().sum();
    println!("{} calls, {} checks: mean {:?}, p50 {:?}, p99 {:?}, max {:?}", rows.len(), scanner.check_ids().len(), total / rows.len() as u32, times[times.len() / 2], times[times.len() * 99 / 100], times.last().unwrap());
}
