//! Turns tool calls a model wrote into its reply text back into real ones.
//!
//! Local models sometimes print a call (`<tool_call>{"name": ...}</tool_call>`) instead of
//! making it, often when the server's parser misses their format, so the harness never runs
//! it and the turn just ends. Two formats heal:
//!
//! - Hermes/Qwen JSON: `<tool_call>{"name": "bash", "arguments": {...}}</tool_call>`, with
//!   arguments as an object or a JSON string.
//! - Qwen3-Coder XML: `<tool_call><function=bash><parameter=command>ls</parameter></function></tool_call>`,
//!   with or without the `<tool_call>` wrapper. Values are typed by the tool's schema.
//!
//! Only names the request declared heal; anything else stays text, as do tags inside
//! Markdown code (a reply explaining the format) and calls cut off before they end. Healed
//! calls go through the same checks as real ones.

use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;

use serde_json::{Map, Value};

use crate::compact_json;
use crate::policy::declared_tools;

const WRAPPER: &str = "<tool_call>";
const WRAPPER_END: &str = "</tool_call>";
const FUNCTION: &str = "<function=";
const FUNCTION_END: &str = "</function>";
const PARAMETER: &str = "<parameter=";
const PARAMETER_END: &str = "</parameter>";
const OPENERS: [&str; 2] = [WRAPPER, FUNCTION];

/// The tools a request declared, with their parameter schemas.
pub(crate) struct Tools {
    schemas: HashMap<String, Value>,
}

impl Tools {
    /// `None` when nothing may heal: no tools declared, or `tool_choice` is `none`.
    pub fn new(request: &Value) -> Option<Self> {
        if request.get("tool_choice").and_then(Value::as_str) == Some("none") {
            return None;
        }
        let mut schemas: HashMap<String, Value> = declared_tools(request).into_iter().map(|name| (name, Value::Null)).collect();
        if schemas.is_empty() {
            return None;
        }
        for tool in request.get("tools").and_then(Value::as_array).into_iter().flatten() {
            let function = tool.get("function").unwrap_or(tool);
            if let (Some(name), Some(schema)) = (function.get("name").and_then(Value::as_str), function.get("parameters")) {
                schemas.insert(name.to_string(), schema.clone());
            }
        }
        Some(Self { schemas })
    }

    fn declares(&self, name: &str) -> bool {
        self.schemas.contains_key(name)
    }

    /// An XML parameter's value: JSON when the schema says it isn't a string and it parses as
    /// JSON, else the text as is.
    fn typed(&self, tool: &str, parameter: &str, value: &str) -> Value {
        let kind = self.schemas.get(tool).and_then(|s| s.get("properties")).and_then(|p| p.get(parameter)).and_then(|p| p.get("type"));
        let kinds: Vec<&str> = match kind {
            Some(Value::String(kind)) => vec![kind],
            Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if kinds.iter().any(|k| matches!(*k, "number" | "integer" | "boolean" | "object" | "array" | "null"))
            && let Ok(parsed) = serde_json::from_str(value.trim())
        {
            return parsed;
        }
        Value::String(value.to_string())
    }
}

/// A call found in text, arguments as a JSON string.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Healed {
    pub name: String,
    pub arguments: String,
}

/// A random 32-character alphanumeric id, the shape llama-server gives tool calls.
pub(crate) fn call_id() -> String {
    const CHARS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let state = RandomState::new();
    (0..32u8).map(|i| CHARS[(state.hash_one(i) % CHARS.len() as u64) as usize] as char).collect()
}

/// Incremental Markdown code-block / inline-code tracker so `TextHealer` stays O(N) over long
/// streams instead of rescanning from byte 0 on every tag or `<`.
#[derive(Default, Clone, Copy)]
enum LineState {
    #[default]
    Indent,
    Mark(char, u8),
    Other,
}

#[derive(Default, Clone, Copy)]
struct CodeTracker {
    fence: Option<char>,
    line_state: LineState,
    inline_open: Option<usize>,
    inline_run: usize,
}

impl CodeTracker {
    fn advance(&mut self, s: &str) {
        for c in s.chars() {
            if c == '\n' {
                if let LineState::Mark(ch, 3) = self.line_state {
                    match self.fence {
                        None => self.fence = Some(ch),
                        Some(mark) if mark == ch => self.fence = None,
                        _ => {}
                    }
                }
                self.line_state = LineState::Indent;
                self.inline_open = None;
                self.inline_run = 0;
                continue;
            }
            self.line_state = match self.line_state {
                LineState::Indent if c.is_whitespace() => LineState::Indent,
                LineState::Indent if matches!(c, '`' | '~') => LineState::Mark(c, 1),
                LineState::Indent => LineState::Other,
                LineState::Mark(ch, 3) => LineState::Mark(ch, 3),
                LineState::Mark(ch, n) if c == ch => LineState::Mark(ch, n + 1),
                LineState::Mark(_, _) | LineState::Other => LineState::Other,
            };
            if c == '`' {
                self.inline_run += 1;
            } else if self.inline_run > 0 {
                self.inline_open = match self.inline_open {
                    Some(n) if n == self.inline_run => None,
                    None => Some(self.inline_run),
                    other => other,
                };
                self.inline_run = 0;
            }
        }
    }

    fn is_in_code(&self) -> bool {
        if self.fence.is_some() {
            return true;
        }
        if self.inline_run > 0 {
            !matches!(self.inline_open, Some(n) if n == self.inline_run)
        } else {
            self.inline_open.is_some()
        }
    }
}

/// Heals one reply's text as it streams in. Text flows on at once, except from where an
/// opening tag could be starting: that is held until it closes into a call or proves not to
/// be one.
#[derive(Default)]
pub(crate) struct TextHealer {
    /// Uncompacted tail of the streamed text.
    text: String,
    /// How much of `text` has been passed on or turned into calls.
    done: usize,
    /// Incremental Markdown code state at byte offset `code_pos` within `text`.
    code: CodeTracker,
    code_pos: usize,
}

impl TextHealer {
    /// Takes the next piece of text; returns the text to pass on now and the calls it completed.
    pub fn feed(&mut self, piece: &str, tools: &Tools) -> (String, Vec<Healed>) {
        self.text.push_str(piece);
        self.drain(tools, false)
    }

    /// The reply ended: heals what can be and returns the rest as text.
    pub fn finish(&mut self, tools: &Tools) -> (String, Vec<Healed>) {
        self.drain(tools, true)
    }

    fn in_code_at(&mut self, pos: usize) -> bool {
        debug_assert!(pos >= self.code_pos);
        if pos > self.code_pos {
            self.code.advance(&self.text[self.code_pos..pos]);
            self.code_pos = pos;
        }
        self.code.is_in_code()
    }

    /// The first opening tag at or after `from` that isn't in Markdown code.
    fn next_opener(&mut self, mut from: usize) -> Option<usize> {
        loop {
            let start = from + OPENERS.iter().filter_map(|tag| self.text[from..].find(tag)).min()?;
            if !self.in_code_at(start) {
                return Some(start);
            }
            from = start + 1;
        }
    }

    fn compact(&mut self) {
        const COMPACT_THRESHOLD: usize = 4096;
        if self.done >= COMPACT_THRESHOLD {
            if self.done > self.code_pos {
                self.code.advance(&self.text[self.code_pos..self.done]);
                self.code_pos = self.done;
            }
            self.text.drain(..self.done);
            self.code_pos = self.code_pos.saturating_sub(self.done);
            self.done = 0;
        }
    }

    fn drain(&mut self, tools: &Tools, at_end: bool) -> (String, Vec<Healed>) {
        let mut out = String::new();
        let mut calls = Vec::new();
        let mut from = self.done;
        loop {
            let Some(start) = self.next_opener(from) else {
                // A tag starting in code wouldn't count, so code like `Vec<` isn't held.
                let held = if at_end { 0 } else { partial_opener(&self.text[from..]) };
                let end = self.text.len() - held;
                let end = if held > 0 && self.in_code_at(end) { self.text.len() } else { end };
                out.push_str(&self.text[self.done..end]);
                self.done = end;
                break;
            };
            match parse(&self.text[start..], tools, at_end) {
                Err(Stop::More) => {
                    out.push_str(&self.text[self.done..start]);
                    self.done = start;
                    break;
                }
                // Not a call: the tag is text. Look for one after its `<`.
                Err(Stop::No) => from = start + 1,
                Ok((call, length)) => {
                    // A whole call to an undeclared tool stays text, and so does anything in it.
                    if tools.declares(&call.name) {
                        out.push_str(&self.text[self.done..start]);
                        calls.push(call);
                        self.done = start + length;
                    }
                    from = start + length;
                }
            }
        }
        self.compact();
        (out, calls)
    }
}

/// Heals a whole reply: the text left over and the calls found.
pub(crate) fn heal_text(text: &str, tools: &Tools) -> (String, Vec<Healed>) {
    let mut healer = TextHealer::default();
    let (mut out, mut calls) = healer.feed(text, tools);
    let (rest, more) = healer.finish(tools);
    out.push_str(&rest);
    calls.extend(more);
    (out, calls)
}

/// Length of the longest end of `text` that could still grow into an opening tag.
fn partial_opener(text: &str) -> usize {
    (1..WRAPPER.len().min(text.len() + 1))
        .rev()
        .filter(|n| text.is_char_boundary(text.len() - n))
        .find(|n| OPENERS.iter().any(|tag| tag.starts_with(&text[text.len() - n..])))
        .unwrap_or(0)
}

/// Why text isn't a call (yet).
#[derive(Debug, PartialEq)]
enum Stop {
    /// It can't be one.
    No,
    /// It could still become one.
    More,
}

/// A call at the start of `text` and its length in bytes. At the end of the reply, a
/// complete call missing only its `</tool_call>` still counts; nothing else that's unfinished does.
fn parse(text: &str, tools: &Tools, at_end: bool) -> Result<(Healed, usize), Stop> {
    let mut c = Cursor { text, pos: 0 };
    let call = if c.text.starts_with(FUNCTION) {
        function_call(&mut c, tools)
    } else {
        wrapped_call(&mut c, tools, at_end)
    };
    match call {
        Err(Stop::More) if at_end => Err(Stop::No),
        Ok(call) => Ok((call, c.pos)),
        Err(stop) => Err(stop),
    }
}

fn wrapped_call(c: &mut Cursor, tools: &Tools, at_end: bool) -> Result<Healed, Stop> {
    c.tag(WRAPPER)?;
    c.skip_space();
    let call = if c.rest().starts_with('{') { json_call(c)? } else { function_call(c, tools)? };
    c.skip_space();
    match c.tag(WRAPPER_END) {
        Err(Stop::More) if at_end => c.pos = c.text.len(),
        result => result?,
    }
    Ok(call)
}

/// `{"name": ..., "arguments": ...}`; some models say `parameters`.
fn json_call(c: &mut Cursor) -> Result<Healed, Stop> {
    let mut values = serde_json::Deserializer::from_str(c.rest()).into_iter::<Value>();
    let value = match values.next() {
        Some(Ok(value)) => value,
        Some(Err(e)) if e.is_eof() => return Err(Stop::More),
        _ => return Err(Stop::No),
    };
    c.pos += values.byte_offset();
    let name = value.get("name").and_then(Value::as_str).ok_or(Stop::No)?;
    let arguments = match value.get("arguments").or_else(|| value.get("parameters")) {
        None => Value::Object(Map::new()),
        Some(Value::String(s)) => serde_json::from_str(s).map_err(|_| Stop::No)?,
        Some(other) => other.clone(),
    };
    if !arguments.is_object() {
        return Err(Stop::No);
    }
    Ok(Healed { name: name.to_string(), arguments: compact_json(&arguments) })
}

/// `<function=NAME><parameter=P>value</parameter>...</function>`.
fn function_call(c: &mut Cursor, tools: &Tools) -> Result<Healed, Stop> {
    c.tag(FUNCTION)?;
    let name = c.name()?;
    let mut arguments = Map::new();
    loop {
        c.skip_space();
        if c.either(FUNCTION_END, PARAMETER)? {
            break;
        }
        let parameter = c.name()?;
        let value = c.until(PARAMETER_END)?;
        // The template puts values on their own lines.
        let value = value.strip_prefix('\n').unwrap_or(value);
        let value = value.strip_suffix('\n').unwrap_or(value);
        arguments.insert(parameter.to_string(), tools.typed(name, parameter, value));
    }
    Ok(Healed { name: name.to_string(), arguments: compact_json(&Value::Object(arguments)) })
}

struct Cursor<'a> {
    text: &'a str,
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn rest(&self) -> &'a str {
        &self.text[self.pos..]
    }

    fn skip_space(&mut self) {
        self.pos = self.text.len() - self.rest().trim_start().len();
    }

    /// Consumes `tag`, or says whether what's left could still become it.
    fn tag(&mut self, tag: &str) -> Result<(), Stop> {
        let rest = self.rest();
        if rest.starts_with(tag) {
            self.pos += tag.len();
            Ok(())
        } else if tag.starts_with(rest) {
            Err(Stop::More)
        } else {
            Err(Stop::No)
        }
    }

    /// Consumes `a` (true) or `b` (false).
    fn either(&mut self, a: &str, b: &str) -> Result<bool, Stop> {
        let first = self.tag(a);
        if first.is_ok() {
            return Ok(true);
        }
        match (first, self.tag(b)) {
            (_, Ok(())) => Ok(false),
            (Err(Stop::More), _) | (_, Err(Stop::More)) => Err(Stop::More),
            _ => Err(Stop::No),
        }
    }

    /// A function or parameter name and the `>` after it.
    fn name(&mut self) -> Result<&'a str, Stop> {
        let rest = self.rest();
        let length = rest.find(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))).ok_or(Stop::More)?;
        if length == 0 || !rest[length..].starts_with('>') {
            return Err(Stop::No);
        }
        self.pos += length + 1;
        Ok(&rest[..length])
    }

    /// The text up to `end`, consuming both.
    fn until(&mut self, end: &str) -> Result<&'a str, Stop> {
        let rest = self.rest();
        let length = rest.find(end).ok_or(Stop::More)?;
        self.pos += length + end.len();
        Ok(&rest[..length])
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn tools() -> Tools {
        Tools::new(&json!({"tools": [
            {"type": "function", "function": {"name": "bash", "parameters": {"type": "object", "properties": {"command": {"type": "string"}, "timeout": {"type": "integer"}}}}},
            {"type": "function", "function": {"name": "edit", "parameters": {"type": "object", "properties": {"edits": {"type": "array"}, "replace_all": {"type": "boolean"}}}}},
        ]}))
        .unwrap()
    }

    fn call(name: &str, arguments: Value) -> Healed {
        Healed { name: name.into(), arguments: compact_json(&arguments) }
    }

    #[test]
    fn json_format_heals_with_object_or_string_arguments() {
        let text = "Listing.\n<tool_call>\n{\"name\": \"bash\", \"arguments\": {\"command\": \"ls\"}}\n</tool_call>\nThen <tool_call>{\"name\": \"bash\", \"arguments\": \"{\\\"command\\\": \\\"pwd\\\"}\"}</tool_call>";
        let (rest, calls) = heal_text(text, &tools());
        assert_eq!(rest, "Listing.\n\nThen ");
        assert_eq!(calls, [call("bash", json!({"command": "ls"})), call("bash", json!({"command": "pwd"}))]);
    }

    #[test]
    fn xml_format_heals_with_typed_values() {
        let text = "<tool_call>\n<function=bash>\n<parameter=command>\necho \"a  b\"\n</parameter>\n<parameter=timeout>\n30\n</parameter>\n</function>\n</tool_call>";
        let (rest, calls) = heal_text(text, &tools());
        assert_eq!(rest, "");
        assert_eq!(calls, [call("bash", json!({"command": "echo \"a  b\"", "timeout": 30}))]);

        // No wrapper; values that don't parse stay strings.
        let text = "<function=edit><parameter=edits>[{\"a\": 1}]</parameter><parameter=replace_all>maybe</parameter></function> done";
        let (rest, calls) = heal_text(text, &tools());
        assert_eq!(rest, " done");
        assert_eq!(calls, [call("edit", json!({"edits": [{"a": 1}], "replace_all": "maybe"}))]);
    }

    #[test]
    fn undeclared_names_stay_text() {
        for text in [
            "<tool_call>{\"name\": \"shell\", \"arguments\": {\"cmd\": \"<function=bash></function>\"}}</tool_call>",
            "<function=shell><parameter=cmd>ls</parameter></function>",
        ] {
            assert_eq!(heal_text(text, &tools()), (text.to_string(), Vec::new()));
        }
    }

    #[test]
    fn mentions_and_unfinished_calls_stay_text() {
        for text in [
            "Models wrap calls in `<tool_call>` tags.",
            "Like `<tool_call>{\"name\": \"bash\", \"arguments\": {}}</tool_call>`.",
            "```\n<tool_call>{\"name\": \"bash\", \"arguments\": {}}</tool_call>\n```",
            "I use <tool_call> and <function=x> tags.",
            "<tool_call>{\"name\": \"bash\", \"arguments\": {\"command\": \"rm -rf bu",
            "<function=bash><parameter=command>ls</parameter>",
        ] {
            assert_eq!(heal_text(text, &tools()), (text.to_string(), Vec::new()), "{text}");
        }
        // After a closed fence, tags count again.
        let (rest, calls) = heal_text("```\nx\n```\n<function=bash></function>", &tools());
        assert_eq!((rest.as_str(), calls.len()), ("```\nx\n```\n", 1));
    }

    #[test]
    fn a_complete_call_missing_its_closing_wrapper_heals_at_the_end() {
        let (rest, calls) = heal_text("<tool_call>{\"name\": \"bash\", \"arguments\": {}}\n", &tools());
        assert_eq!((rest.as_str(), calls), ("", vec![call("bash", json!({}))]));
    }

    #[test]
    fn stream_holds_only_what_could_be_a_tag() {
        let tools = tools();
        let mut healer = TextHealer::default();
        let mut sent = String::new();
        let mut calls = Vec::new();
        for piece in ["a < b, ", "see <to", "ol_call>", "{\"name\": \"bash\", \"arguments\": {\"command\": \"ls\"}}</tool", "_call> ok <"] {
            let (text, healed) = healer.feed(piece, &tools);
            sent.push_str(&text);
            calls.extend(healed);
        }
        assert_eq!(sent, "a < b, see  ok ");
        assert_eq!(calls, [call("bash", json!({"command": "ls"}))]);
        assert_eq!(healer.finish(&tools).0, "<");
    }

    #[test]
    fn ids_look_like_llama_servers() {
        let (a, b) = (call_id(), call_id());
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(a, b);
    }

    #[test]
    fn nothing_heals_without_tools_or_with_tool_choice_none() {
        assert!(Tools::new(&json!({"messages": []})).is_none());
        assert!(Tools::new(&json!({"tools": [{"type": "function", "function": {"name": "bash"}}], "tool_choice": "none"})).is_none());
    }

    #[test]
    fn incremental_code_tracker_compacts_and_handles_fences_and_inline_spans() {
        let tools = tools();
        let mut healer = TextHealer::default();
        // Large fenced block containing many `<` and fake tags, then a real call outside code.
        let mut stream = String::from("```rust\n");
        for _ in 0..400 {
            stream.push_str("let v: Vec<Option<String>> = <tool_call>{\"name\":\"bash\",\"arguments\":{}}</tool_call>;\n");
        }
        stream.push_str("```\nNow ``inline `code` ``<function=bash><parameter=command>echo ok</parameter></function> and `Vec<`");
        let mut sent = String::new();
        let mut calls = Vec::new();
        for chunk in stream.as_bytes().chunks(13) {
            let piece = std::str::from_utf8(chunk).unwrap();
            let (text, healed) = healer.feed(piece, &tools);
            sent.push_str(&text);
            calls.extend(healed);
        }
        let (tail, more) = healer.finish(&tools);
        sent.push_str(&tail);
        calls.extend(more);
        assert_eq!(calls, [call("bash", json!({"command": "echo ok"}))]);
        assert!(sent.starts_with("```rust\n"));
        assert!(sent.ends_with("Now ``inline `code` `` and `Vec<`"));
        assert!(healer.text.len() < 8192, "buffer should compact: {} bytes", healer.text.len());
    }
}
