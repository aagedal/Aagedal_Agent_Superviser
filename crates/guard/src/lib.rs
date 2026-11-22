//! A local proxy between a coding harness and its model server that checks each tool call
//! the model makes before the harness runs it. Blocked calls are replaced with a text notice,
//! so the harness runs nothing.
//!
//! Ported from [safeagents](https://github.com/huggingface/safeagents) (Apache-2.0):
//! the request-only checks (undeclared tools, hosts from nowhere), self-protection, the
//! journal and the response/stream rewriting. atif-scan's detectors run through
//! `superviser-scan`, and suspicious calls go to a judge model. Calls a model wrote into its
//! reply text instead of making them are healed into real ones first, and checked the same.

pub mod guard;
mod heal;
pub mod journal;
pub mod judge;
pub mod policy;
pub mod project;
pub mod proxy;
pub mod rewrite;
pub mod selfprotect;

pub use guard::{CallEvent, Config, Guard, Review, Verdict};
pub use judge::{Judge, JudgeOptions};
pub use project::ProjectRules;
pub use proxy::{GuardServer, ProxyOptions};

/// `value` as JSON without whitespace, non-ASCII kept as is.
pub(crate) fn compact_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value).expect("JSON values serialize")
}
