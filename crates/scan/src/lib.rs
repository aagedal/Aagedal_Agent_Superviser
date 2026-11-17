//! Static checks on a coding agent's pending tool calls: a Rust port of the local-safety
//! checks of [atif-scan](https://github.com/huggingface/atif-scan) (MIT), as safeagents
//! runs them. See `NOTICE.md` for what is ported and `parity/` for how it is kept equal.
//!
//! The conversation becomes a trace whose last agent step holds the pending calls; only
//! findings with evidence on the last of those calls are reported.

pub mod credentials;
pub mod detectors;
pub mod model;
pub mod net;
pub mod outbound;
pub mod pysrc;
pub mod re;
pub mod shell;
pub mod tools;
pub mod trace;
pub mod webhooks;

use serde::Serialize;
use serde_json::Value;

use crate::detectors::Detector;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Copy)]
pub struct CheckSpec {
    pub id: &'static str,
    pub severity: Severity,
    pub title: &'static str,
}

/// A finding on the pending call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Signal {
    pub id: &'static str,
    pub severity: Severity,
    pub title: &'static str,
}

impl std::fmt::Display for Signal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.id, serde_json::to_value(self.severity).unwrap().as_str().unwrap())
    }
}

pub struct Scanner {
    detectors: Vec<Box<dyn Detector>>,
}

impl Default for Scanner {
    fn default() -> Self {
        Self::new()
    }
}

impl Scanner {
    /// Every ported check.
    pub fn new() -> Self {
        Self { detectors: detectors::builtin() }
    }

    /// The ids of the checks this scanner runs.
    pub fn check_ids(&self) -> Vec<&'static str> {
        self.detectors.iter().map(|d| d.spec().id).collect()
    }

    /// Findings on the last of `pending` (name, raw JSON arguments), judged after the
    /// conversation in `request`. Informational findings are left out.
    pub fn signals(&self, request: &Value, pending: &[(String, String)]) -> Vec<Signal> {
        let Some(last_call) = pending.len().checked_sub(1) else { return Vec::new() };
        let trace = trace::build(request, pending);
        let last_step = trace.steps.len() - 1;
        let mut signals = Vec::new();
        for detector in &self.detectors {
            let spec = detector.spec();
            if spec.severity <= Severity::Info {
                continue;
            }
            let hits = detector.evaluate(&trace);
            if hits.iter().any(|at| at.step == last_step && at.call == Some(last_call)) {
                signals.push(Signal { id: spec.id, severity: spec.severity, title: spec.title });
            }
        }
        signals
    }
}
