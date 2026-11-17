//! Every ported check finds exactly what Python safeagents' scanner found, on every call of
//! the corpus (see `parity/README.md`).

use std::collections::BTreeSet;

use serde_json::Value;
use superviser_scan::Scanner;

fn lines(name: &str) -> Vec<Value> {
    let path = format!("{}/parity/data/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}")).lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

#[test]
fn findings_match_python() {
    let scanner = Scanner::new();
    let ported: BTreeSet<&str> = scanner.check_ids().into_iter().collect();
    let expected = lines("expected.jsonl");
    let mut mismatches = Vec::new();
    let mut compared = 0;
    let mut positives = 0;
    for (row, expected) in lines("corpus.jsonl").iter().zip(&expected) {
        assert_eq!(row["id"], expected["id"]);
        let want: BTreeSet<String> = expected["findings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["id"].as_str().unwrap().to_string())
            .filter(|id| ported.contains(id.as_str()))
            .collect();
        let pending = [(row["name"].as_str().unwrap().to_string(), row["arguments"].as_str().unwrap().to_string())];
        let got: BTreeSet<String> = scanner.signals(&row["request"], &pending).into_iter().map(|s| s.id.to_string()).collect();
        compared += 1;
        positives += want.len();
        if got != want {
            let args: String = row["arguments"].as_str().unwrap().chars().take(160).collect();
            mismatches.push(format!("{}: python {want:?}, rust {got:?}\n    {args}", row["id"]));
        }
    }
    let shown = mismatches.iter().take(40).cloned().collect::<Vec<_>>().join("\n");
    assert!(mismatches.is_empty(), "{} of {compared} calls differ on {} ported checks:\n{shown}", mismatches.len(), ported.len());
    // Guard against comparing nothing: the corpus holds hundreds of findings of ported checks.
    assert!(positives >= 500, "only {positives} expected findings of ported checks");
}

#[test]
fn shell_reader_matches_python() {
    use superviser_scan::shell;
    let mut mismatches = Vec::new();
    let rows = lines("shell.jsonl");
    for row in &rows {
        let text = row["text"].as_str().unwrap();
        let script = shell::parse(text);
        let slice = |spans: Vec<(usize, usize)>| -> Vec<String> { spans.into_iter().map(|(s, e)| text[s..e].to_string()).collect() };
        let got = serde_json::json!({
            "text": text,
            "complete": script.complete,
            "names": script.commands().map(|c| c.name()).collect::<Vec<_>>(),
            "background": script.commands().map(|c| c.background).collect::<Vec<_>>(),
            "repeated": script.commands().map(|c| c.repeated).collect::<Vec<_>>(),
            "bodies": slice(script.bodies.clone()),
            "file_targets": script.file_targets,
            "unused_file_bodies": slice(shell::unused_file_bodies(&script)),
            "literal_search_patterns": slice(shell::literal_search_patterns(text)),
        });
        if &got != row {
            let differing: Vec<&str> = ["complete", "names", "background", "repeated", "bodies", "file_targets", "unused_file_bodies", "literal_search_patterns"]
                .into_iter()
                .filter(|k| got[k] != row[k])
                .collect();
            let first = differing[0];
            mismatches.push(format!("{differing:?}\n    python {}\n    rust   {}\n    {:?}", row[first], got[first], text.chars().take(200).collect::<String>()));
        }
    }
    let shown = mismatches.iter().take(15).cloned().collect::<Vec<_>>().join("\n");
    assert!(mismatches.is_empty(), "{} of {} commands read differently:\n{shown}", mismatches.len(), rows.len());
}
