//! A local proxy between a coding harness and its model server that checks each tool call
//! the model makes before the harness runs it. Blocked calls are replaced with a text notice,
//! so the harness runs nothing.
//!
//! Ported from [safeagents](https://github.com/huggingface/safeagents) (Apache-2.0):
//! the request-only checks (undeclared tools, hosts from nowhere), self-protection, the
//! journal and the response/stream rewriting. atif-scan's detectors and the judge model are
//! not ported yet.

pub mod guard;
pub mod journal;
pub mod policy;
pub mod proxy;
pub mod rewrite;
pub mod selfprotect;

pub use guard::{CallEvent, Config, Guard, Review, Verdict};
pub use proxy::{GuardServer, ProxyOptions};

/// `value` as JSON without whitespace, non-ASCII kept as is.
pub(crate) fn compact_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value).expect("JSON values serialize")
}
