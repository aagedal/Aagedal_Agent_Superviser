//! The trace a request's conversation becomes, with the response's pending calls as its
//! last agent step. Port of safeagents' `scan.history`/`recent`/`call_step` and the parts
//! of atif-scan's loader they reach. Reasoning is left out: a model's account of what it
//! is doing is not evidence of what it does.

use serde_json::{Map, Value};

use crate::model::{Call, Content, Observation, Source, Step, Trace};
use crate::tools;

/// Agent turns kept from the conversation (user and system turns all stay).
const RECENT_AGENT_STEPS: usize = 20;

/// Text of a message `content`: a string, or the text parts of a list.
pub fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|p| matches!(p.get("type").and_then(Value::as_str), Some("text" | "input_text")))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// A raw call before classification.
struct RawCall {
    id: String,
    name: String,
    arguments: Map<String, Value>,
}

/// Arguments as safeagents passes them on: a JSON object, else `{"input": text}`.
fn arguments(args: Option<&Value>) -> Map<String, Value> {
    let parsed = match args {
        Some(Value::String(s)) => serde_json::from_str::<Value>(s).unwrap_or_else(|_| {
            let mut map = Map::new();
            map.insert("input".into(), Value::String(s.clone()));
            Value::Object(map)
        }),
        Some(other) => other.clone(),
        None => Value::Object(Map::new()),
    };
    match parsed {
        Value::Object(map) => map,
        other => {
            let mut map = Map::new();
            map.insert("input".into(), Value::String(other.to_string()));
            map
        }
    }
}

fn raw_call(id: Option<&Value>, name: Option<&Value>, args: Option<&Value>) -> RawCall {
    let text = |v: Option<&Value>| match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => "None".to_string(),
        Some(other) => other.to_string(),
    };
    RawCall { id: text(id), name: text(name), arguments: arguments(args) }
}

enum RawStep {
    System(String),
    User(String),
    Agent { message: String, calls: Vec<RawCall>, results: Vec<(String, String)> },
}

/// The steps of an OpenAI or Anthropic request's conversation, without reasoning.
fn history(request: &Value) -> Vec<RawStep> {
    let mut steps: Vec<RawStep> = Vec::new();
    // A tool result joins the last agent step, if it made calls.
    fn observe(steps: &mut [RawStep], call_id: Option<&Value>, content: Option<&Value>) {
        if let Some(RawStep::Agent { calls, results, .. }) = steps.iter_mut().rev().find(|s| matches!(s, RawStep::Agent { .. }))
            && !calls.is_empty()
        {
            let id = match call_id {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Null) | None => "None".into(),
                Some(other) => other.to_string(),
            };
            results.push((id, content_text(content)));
        }
    }
    let system = content_text(request.get("system"));
    if !system.is_empty() {
        steps.push(RawStep::System(system));
    }
    for message in request.get("messages").and_then(Value::as_array).into_iter().flatten() {
        let Some(message) = message.as_object() else { continue };
        let content = message.get("content");
        let blocks: &[Value] = content.and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
        match message.get("role").and_then(Value::as_str) {
            Some("system" | "developer") => steps.push(RawStep::System(content_text(content))),
            Some("user") => {
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                        observe(&mut steps, block.get("tool_use_id"), block.get("content"));
                    }
                }
                let text = content_text(content);
                if !text.is_empty() {
                    steps.push(RawStep::User(text));
                }
            }
            Some("assistant") => {
                let mut calls = Vec::new();
                for call in message.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
                    if let Some(function) = call.get("function").filter(|f| f.is_object()) {
                        calls.push(raw_call(call.get("id"), function.get("name"), function.get("arguments")));
                    }
                }
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                        calls.push(raw_call(block.get("id"), block.get("name"), block.get("input")));
                    }
                }
                steps.push(RawStep::Agent { message: content_text(content), calls, results: Vec::new() });
            }
            Some("tool") => observe(&mut steps, message.get("tool_call_id"), content),
            _ => {}
        }
    }
    steps
}

/// The steps with only the last `RECENT_AGENT_STEPS` agent turns.
fn recent(steps: Vec<RawStep>) -> Vec<RawStep> {
    let agents = steps.iter().filter(|s| matches!(s, RawStep::Agent { .. })).count();
    let mut drop = agents.saturating_sub(RECENT_AGENT_STEPS);
    steps
        .into_iter()
        .filter(|s| {
            if drop > 0 && matches!(s, RawStep::Agent { .. }) {
                drop -= 1;
                return false;
            }
            true
        })
        .collect()
}

fn call(index: usize, raw: RawCall) -> Call {
    let tool = tools::normalize_tool(&raw.name, Some(&raw.arguments));
    let fields = tools::call_fields(tool, Some(&raw.arguments), &raw.name);
    Call { index, tool, fields, id: raw.id, name: raw.name, arguments: Some(raw.arguments) }
}

/// The trace for `request`'s conversation, ending in an agent step holding `pending` calls
/// (name, raw JSON arguments).
pub fn build(request: &Value, pending: &[(String, String)]) -> Trace {
    let mut raw = recent(history(request));
    let calls = pending
        .iter()
        .enumerate()
        .map(|(i, (name, args))| RawCall { id: format!("pending-{i}"), name: name.clone(), arguments: arguments(Some(&Value::String(args.clone()))) })
        .collect();
    raw.push(RawStep::Agent { message: String::new(), calls, results: Vec::new() });
    let steps = raw
        .into_iter()
        .enumerate()
        .map(|(index, step)| match step {
            RawStep::System(text) => Step { index, source: Source::System, message: Content::text(text), reasoning: Content::default(), calls: vec![], observations: vec![] },
            RawStep::User(text) => Step { index, source: Source::User, message: Content::text(text), reasoning: Content::default(), calls: vec![], observations: vec![] },
            RawStep::Agent { message, calls, results } => Step {
                index,
                source: Source::Agent,
                message: Content::text(message),
                reasoning: Content::default(),
                calls: calls.into_iter().enumerate().map(|(i, c)| call(i, c)).collect(),
                observations: results.into_iter().map(|(id, text)| Observation { source_call_id: Some(id), content: Content::text(text) }).collect(),
            },
        })
        .collect();
    Trace { steps }
}
