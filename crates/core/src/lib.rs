//! Everything Agent Superviser does that isn't UI: picking models that fit this machine,
//! downloading them from the Hugging Face Hub, running the inference server, and pointing
//! coding harnesses at the guard.

pub mod agent;
pub mod awake;
pub mod catalog;
pub mod discover;
pub mod gguf;
pub mod hardware;
pub mod hub;
pub mod installed;
pub mod opencode;
pub mod paths;
pub mod project;
pub mod quant;
pub mod runtime;
pub mod sandbox;
pub mod secrets;
