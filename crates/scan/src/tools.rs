//! Tool names normalised to categories, and argument values classified into channels.
//! Port of atif-scan's `data/tools.py`, without tool programs (Codex code mode,
//! fast-agent `execute_python`), whose readers (`jslit`, `pylit`) aren't ported.

use serde_json::{Map, Value};

use crate::lazy_re;
use crate::model::{Channel, Content, Field};
use crate::re::{Lazy, full};

const TOOLS: &[(&str, &[&str])] = &[
    ("shell", &["bash", "Bash", "shell", "Shell", "exec", "execute", "execute_command", "run_command", "run_script", "run_shell_command", "terminal"]),
    ("read", &["read_text_file", "read_file", "read", "Read", "view_file"]),
    ("write", &["write_text_file", "write_file", "write", "Write", "edit", "Edit", "edit_text", "edit_file", "str_replace"]),
    ("search_files", &["grep", "Grep", "glob", "Glob", "find_file_by_name", "list_files"]),
    ("web_fetch", &["web_fetch", "fetch_url", "webfetch", "WebFetch", "fetch"]),
    ("web_search", &["web_search", "web_search_preview", "WebSearch"]),
    ("inert", &["todo_write", "TodoWrite", "get_output", "kill_shell", "service_status", "ToolSearch"]),
];

/// Harness-specific names (atif-scan's `HARNESS_ALIASES`).
const HARNESS_ALIASES: &[(&str, &str)] = &[
    ("FetchURL", "web_fetch"), ("TodoList", "inert"),
    ("shellToolCall", "shell"), ("readToolCall", "read"), ("editToolCall", "write"), ("writeToolCall", "write"),
    ("deleteToolCall", "write"), ("grepToolCall", "search_files"), ("globToolCall", "search_files"),
    ("webFetchToolCall", "web_fetch"), ("webSearchToolCall", "web_search"), ("awaitToolCall", "inert"),
    ("updateTodosToolCall", "inert"), ("write_to_process", "shell"), ("start_service", "shell"),
    ("search_code", "search_files"), ("query_code", "search_files"), ("view_image", "read"), ("ocr_pdf", "read"),
    ("stop_service", "inert"), ("service_logs", "inert"), ("wait_tasks", "inert"), ("verify_and_record", "inert"),
    ("task_acceptance_review", "inert"), ("schedule_subagent", "inert"), ("skill", "inert"),
    ("exec_command", "shell"), ("write_stdin", "shell"), ("apply_patch", "write"), ("update_plan", "inert"),
    ("update_goal", "inert"), ("wait", "inert"), ("bash_command", "shell"), ("mark_task_complete", "inert"),
    ("process", "inert"), ("attach_media", "read"), ("ReadSnippets", "read"), ("MultiEdit", "write"),
    ("SourcePack", "search_files"), ("Diff", "read"), ("Todo", "inert"), ("GitStatusInspect", "inert"),
    ("RunVerification", "inert"), ("TaskCreate", "inert"), ("TaskUpdate", "inert"), ("TaskStop", "inert"),
    ("mcp__lc__bash", "shell"), ("mcp__lc__read", "read"), ("mcp__lc__edit", "write"), ("mcp__lc__code_search", "search_files"),
    ("run_terminal_command", "shell"), ("search_replace", "write"), ("list_dir", "search_files"),
    ("get_terminal_command_output", "inert"), ("kill_terminal_command", "inert"), ("background_exec", "shell"),
    ("plan", "inert"), ("read_tool_result_page", "inert"), ("replace", "write"), ("grep_search", "search_files"),
    ("list_directory", "search_files"), ("update_topic", "inert"), ("google_web_search", "web_search"),
    ("read_background_output", "inert"), ("list_background_processes", "inert"), ("ls", "search_files"),
    ("find", "search_files"), ("rg", "search_files"), ("fd", "search_files"), ("read_symbol", "read"),
    ("multi_edit", "write"), ("http", "web_fetch"), ("git_diff", "read"), ("git_log", "read"),
    ("awk", "search_files"), ("jq", "search_files"), ("csvkit", "search_files"), ("todo_read", "inert"),
    ("pdf_parse", "read"), ("todowrite", "inert"), ("invalid", "inert"), ("tool_search", "inert"),
    ("image_to_text", "read"), ("task_output", "inert"), ("task_get", "inert"), ("task_stop", "inert"),
    ("sleep", "inert"), ("goal_complete", "inert"), ("goal_blocked", "inert"), ("tb_bridge.task", "inert"),
    ("tb_bridge.upload", "write"), ("tb_bridge.finalize", "inert"),
];

fn alias(name: &str) -> Option<&'static str> {
    HARNESS_ALIASES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, c)| *c)
        .or_else(|| TOOLS.iter().find(|(_, names)| names.contains(&name)).map(|(c, _)| *c))
}

/// The channel a tool category must have.
fn required(tool: &str) -> Option<Channel> {
    Some(match tool {
        "shell" => Channel::Command,
        "read" | "write" => Channel::Path,
        "web_fetch" => Channel::Url,
        "web_search" => Channel::Query,
        _ => return None,
    })
}

const COMMAND_KEYS: &[&str] = &["command", "cmd", "commands", "script", "shellcommand", "bash", "code", "textinput", "keystrokes", "chars"];
const TERMINAL_INPUT_KEYS: &[&str] = &["textinput", "keystrokes", "chars"];
const QUERY_KEYS: &[&str] = &["query", "q", "searchquery", "searchterm", "queries"];
const URL_KEYS: &[&str] = &["url", "urls", "uri", "href", "link", "endpoint"];
const PATH_KEYS: &[&str] = &[
    "targetdirectory", "path", "paths", "filepath", "file", "files", "filename", "dir", "directory", "cwd", "workdir",
    "workingdirectory", "source", "src", "target", "destination", "dest", "output", "outputpath", "root", "notebookpath",
];
const PAYLOAD_KEYS: &[&str] = &[
    "bytesinput", "input", "globpattern", "streamcontent", "content", "contents", "text", "body", "data", "newstring",
    "newstr", "newtext", "insertline", "patch", "diff", "edits", "description", "prompt", "message", "messages",
    "instructions", "note", "notes", "todos", "summary", "reasoning", "thought", "explanation", "title", "pattern",
    "regex", "glob", "include", "exclude",
];
/// Existing text an edit quotes (not authored by the agent): not evidence of anything.
const QUOTED_KEYS: &[&str] = &["oldstring", "oldstr", "oldtext", "original", "search", "find"];
/// Edit-tool keys holding the new text but not read as payload.
const CONTENT_KEYS: &[&str] = &["replace", "replacement", "new", "patchtext"];
/// Argument keys naming a file on the harness host copied into the sandbox.
const HOST_PATH_KEYS: &[&str] = &["hostpath", "localpath", "hostsource", "localsource"];

static URL_VALUE: Lazy = std::sync::LazyLock::new(|| full(r"[a-zA-Z][a-zA-Z0-9+.-]*://\S+"));
static PATH_VALUE: Lazy = std::sync::LazyLock::new(|| full(r"(?:/|~/|\./|\.\./|[A-Za-z]:[\\/])[^\n]{0,4095}"));
static PATCH_ENVELOPE: Lazy = lazy_re!(r"(?m)^\*\*\* Begin Patch\b");
static PATCH_PATHS: Lazy = lazy_re!(r"(?m)^\*\*\* (?:Add File|Update File|Delete File|Move to): *(\S.*)$");
static NAMESPACE: Lazy = lazy_re!(r"^[A-Za-z_][\w-]*(?:\.|:|__)(\w+)$");
static HOST_PATH_VALUE: Lazy = lazy_re!(r"^(?:[A-Za-z]:[\\/]|\\\\[^\\\s])");

/// Model-authored prose tools: the key holding the prose, and its channel.
fn prose_tool(name: &str) -> Option<(&'static str, Channel)> {
    match name {
        "think" => Some(("thought", Channel::Reasoning)),
        "finish" => Some(("message", Channel::Message)),
        "create_goal" => Some(("objective", Channel::Message)),
        _ => None,
    }
}

/// Tools whose `action_type` (or `command`) argument says what they did.
fn action_tool(name: &str, action: &str) -> Option<&'static str> {
    Some(match (name, action) {
        ("web_search_call", "search") => "web_search",
        ("web_search_call", "open_page" | "find_in_page") => "web_fetch",
        ("file_editor", "view") => "read",
        ("file_editor", "create" | "str_replace" | "insert" | "undo_edit") => "write",
        ("task_tracker", "view" | "plan") => "inert",
        _ => return None,
    })
}

fn action_key(name: &str) -> Option<&'static str> {
    match name {
        "web_search_call" => Some("action_type"),
        "file_editor" | "task_tracker" => Some("command"),
        _ => None,
    }
}

const NO_INPUT_TOOLS: &[&str] = &["clock__curr_time", "get_goal"];
const RAW_INPUT_KEY: &str = "input";

fn raw_input_channel(tool: &str) -> Option<Channel> {
    Some(match tool {
        "shell" => Channel::Command,
        "read" => Channel::Path,
        "write" => Channel::Payload,
        "web_fetch" => Channel::Url,
        "web_search" => Channel::Query,
        _ => return None,
    })
}

pub(crate) fn norm(key: Option<&str>) -> String {
    key.map(|k| k.to_lowercase().chars().filter(|c| !matches!(c, '_' | '-') && !c.is_whitespace()).collect()).unwrap_or_default()
}

fn in_keys(key: Option<&str>, keys: &[&str]) -> bool {
    keys.contains(&norm(key).as_str())
}

/// The raw text of a non-JSON tool call: a lone `input` string.
pub fn raw_input(args: &Map<String, Value>) -> Option<&str> {
    if args.len() == 1 {
        return args.get(RAW_INPUT_KEY).and_then(Value::as_str);
    }
    None
}

pub fn normalize_tool(name: &str, args: Option<&Map<String, Value>>) -> &'static str {
    if name == "web__run" || name == "web.run" {
        let has = |k: &str| args.is_some_and(|a| a.contains_key(k));
        return if has("search_query") {
            "web_search"
        } else if has("open") || has("click") || has("find") {
            "web_fetch"
        } else {
            "other"
        };
    }
    if idle_call(name, args) { "inert" } else { normalize_action(name, args) }
}

/// A call that, with these arguments, sends nothing anywhere.
fn idle_call(name: &str, args: Option<&Map<String, Value>>) -> bool {
    let Some(args) = args else { return false };
    let truthy = |v: Option<&Value>| match v {
        None | Some(Value::Null) => false,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Bool(b)) => *b,
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
    };
    match name {
        "write_stdin" => !truthy(args.get("chars")),
        "background_exec" => !truthy(args.get("command")),
        _ => NO_INPUT_TOOLS.contains(&name) && args.is_empty(),
    }
}

fn normalize_action(name: &str, args: Option<&Map<String, Value>>) -> &'static str {
    if let Some((key, _)) = prose_tool(name) {
        return if is_prose(args, key) { "inert" } else { "other" };
    }
    if let Some(key) = action_key(name) {
        let action = args.and_then(|a| a.get(key)).and_then(Value::as_str);
        return action.and_then(|a| action_tool(name, a)).unwrap_or("other");
    }
    if let Some(category) = alias(name) {
        return category;
    }
    NAMESPACE
        .captures(name)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
        .and_then(|inner: String| alias(&inner))
        .unwrap_or("other")
}

fn is_prose(args: Option<&Map<String, Value>>, key: &str) -> bool {
    args.is_some_and(|a| a.keys().all(|k| k == key || k == "kind") && a.get(key).is_some_and(Value::is_string))
}

/// The call types text into a terminal (a top-level keystrokes-style argument).
pub fn types_into_terminal(args: Option<&Map<String, Value>>) -> bool {
    args.is_some_and(|a| a.keys().any(|k| in_keys(Some(k), TERMINAL_INPUT_KEYS)))
}

/// A leaf of the arguments: a string, a joined argv, or something not understood.
enum Leaf<'a> {
    Text(String),
    /// A list under a command key that isn't all strings (`["bash", 1]`).
    MixedCommand,
    Other(#[allow(dead_code)] &'a Value),
}

/// (nearest key, leaf) for every non-object/list value; list items inherit their key.
fn leaves<'a>(value: &'a Value, key: Option<&'a str>, out: &mut Vec<(Option<&'a str>, Leaf<'a>)>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                leaves(v, Some(k), out);
            }
        }
        Value::Array(items) => {
            if in_keys(key, COMMAND_KEYS) {
                // argv form, e.g. ["bash", "-lc", "..."]; mixed lists are not understood.
                if !items.is_empty() && items.iter().all(Value::is_string) {
                    let joined = items.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ");
                    out.push((key, Leaf::Text(joined)));
                } else {
                    out.push((key, Leaf::MixedCommand));
                }
                return;
            }
            for item in items {
                leaves(item, key, out);
            }
        }
        Value::String(s) => out.push((key, Leaf::Text(s.clone()))),
        other => out.push((key, Leaf::Other(other))),
    }
}

/// Route one argument string by key convention and value shape.
pub fn classify(key: Option<&str>, value: &str) -> (Channel, String) {
    let k = norm(key);
    if PAYLOAD_KEYS.contains(&k.as_str()) {
        return (Channel::Payload, value.to_string());
    }
    if let Some(path) = value.strip_prefix("file://") {
        return (Channel::Path, path.to_string());
    }
    (value_channel(&k, value), value.to_string())
}

fn value_channel(k: &str, value: &str) -> Channel {
    if URL_VALUE.is_match(value) {
        return Channel::Url;
    }
    for (keys, channel) in [(COMMAND_KEYS, Channel::Command), (QUERY_KEYS, Channel::Query), (URL_KEYS, Channel::Url), (PATH_KEYS, Channel::Path)] {
        if keys.contains(&k) {
            return channel;
        }
    }
    if PATH_VALUE.is_match(value) { Channel::Path } else { Channel::Arguments }
}

/// The text's field, plus the file paths inside an apply_patch payload.
fn text_fields(channel: Channel, text: String) -> Vec<Field> {
    let mut fields = Vec::new();
    if channel == Channel::Payload && PATCH_ENVELOPE.is_match(&text) {
        let paths: Vec<String> = PATCH_PATHS.find_all(&text).into_iter().map(|p| p.trim_end().to_string()).collect();
        fields.push((channel, Content::text(text)));
        fields.extend(paths.into_iter().map(|p| (Channel::Path, Content::text(p))));
    } else {
        fields.push((channel, Content::text(text)));
    }
    fields
}

fn leaf_fields(key: Option<&str>, leaf: Leaf) -> Vec<Field> {
    match leaf {
        Leaf::Text(text) => {
            let (channel, text) = classify(key, &text);
            text_fields(channel, text)
        }
        Leaf::MixedCommand => vec![(Channel::Command, Content::unknown())],
        Leaf::Other(_) => Vec::new(),
    }
}

fn argument_fields(tool: &str, args: &Map<String, Value>) -> Vec<Field> {
    let value = Value::Object(args.clone());
    let mut found = Vec::new();
    leaves(&value, None, &mut found);
    let mut fields: Vec<Field> = found
        .into_iter()
        .filter(|(key, _)| !in_keys(*key, QUOTED_KEYS))
        .flat_map(|(key, leaf)| leaf_fields(key, leaf))
        .collect();
    if let Some(required) = required(tool)
        && fields.iter().all(|(c, _)| *c != required)
    {
        fields.push((required, Content::unknown()));
    }
    if tool == "other" && fields.is_empty() {
        fields.push((Channel::Arguments, Content::unknown()));
    }
    fields
}

fn generic_call_fields(tool: &str, args: Option<&Map<String, Value>>) -> Vec<Field> {
    if tool == "inert" {
        return Vec::new();
    }
    let Some(args) = args else {
        // Unparseable arguments could hold anything.
        return vec![(required(tool).unwrap_or(Channel::Arguments), Content::unknown())];
    };
    if let (Some(text), Some(channel)) = (raw_input(args), raw_input_channel(tool)) {
        return text_fields(channel, text.to_string());
    }
    argument_fields(tool, args)
}

/// Generic evidence extraction, with adapters for overloaded tool fields.
pub fn call_fields(tool: &str, args: Option<&Map<String, Value>>, name: &str) -> Vec<Field> {
    if name == "file_editor" {
        return editor_fields(tool, args);
    }
    if let Some((key, channel)) = prose_tool(name)
        && tool == "inert"
    {
        let text = args.and_then(|a| a.get(key)).and_then(Value::as_str).unwrap_or("");
        return vec![(channel, Content::text(text))];
    }
    let mut fields = generic_call_fields(tool, args);
    if (prose_tool(name).is_some() || action_key(name).is_some()) && tool == "other" {
        return unknown_action(fields);
    }
    if tool == "write" && name != "deleteToolCall" && contents_unrecorded(args, &fields) {
        fields.push((Channel::Payload, Content::unknown()));
    }
    fields
}

/// Arguments naming a file on the harness host: by key on any call, by a Windows shape
/// on a write (copied into the sandbox).
pub fn host_paths(tool: &str, args: Option<&Map<String, Value>>) -> Vec<String> {
    let Some(args) = args else { return Vec::new() };
    let value = Value::Object(args.clone());
    let mut found = Vec::new();
    leaves(&value, None, &mut found);
    found
        .into_iter()
        .filter_map(|(key, leaf)| match leaf {
            Leaf::Text(text) if in_keys(key, HOST_PATH_KEYS) || (tool == "write" && HOST_PATH_VALUE.is_match(&text)) => Some(text),
            _ => None,
        })
        .collect()
}

/// A write recording a path but neither its contents nor a host file they came from.
fn contents_unrecorded(args: Option<&Map<String, Value>>, fields: &[Field]) -> bool {
    let Some(args) = args else { return false };
    if fields.iter().any(|(c, _)| *c == Channel::Payload) {
        return false;
    }
    let value = Value::Object(args.clone());
    let mut found = Vec::new();
    leaves(&value, None, &mut found);
    if found.iter().any(|(key, leaf)| in_keys(*key, CONTENT_KEYS) && matches!(leaf, Leaf::Text(_))) {
        return false;
    }
    host_paths("write", Some(args)).is_empty()
}

fn unknown_action(mut fields: Vec<Field>) -> Vec<Field> {
    if fields.iter().any(|(c, v)| *c == Channel::Arguments && !v.understood) {
        return fields;
    }
    fields.push((Channel::Arguments, Content::unknown()));
    fields
}

fn editor_fields(tool: &str, args: Option<&Map<String, Value>>) -> Vec<Field> {
    let Some(args) = args else { return generic_call_fields(tool, None) };
    // `command` is an operation selector, not shell code.
    let adapted: Map<String, Value> = args.iter().filter(|(k, _)| *k != "command" && *k != "file_text").map(|(k, v)| (k.clone(), v.clone())).collect();
    let mut fields = argument_fields(tool, &adapted);
    let one = |key: &str, v: Option<&Value>| -> Vec<Field> {
        match v {
            Some(Value::String(s)) => leaf_fields(Some(key), Leaf::Text(s.clone())),
            _ => Vec::new(),
        }
    };
    fields.extend(one("content", args.get("file_text")));
    if tool == "other" {
        fields.extend(one("action", args.get("command")));
        return unknown_action(fields);
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fields(name: &str, args: Value) -> Vec<(Channel, String, bool)> {
        let args = args.as_object().unwrap();
        let tool = normalize_tool(name, Some(args));
        call_fields(tool, Some(args), name).into_iter().map(|(c, v)| (c, v.text, v.understood)).collect()
    }

    #[test]
    fn opencode_tools() {
        assert_eq!(normalize_tool("bash", None), "shell");
        assert_eq!(normalize_tool("webfetch", None), "web_fetch");
        assert_eq!(normalize_tool("mcp__server__bash", None), "shell");
        assert_eq!(normalize_tool("list", None), "other");
        assert_eq!(
            fields("bash", json!({"command": "ls -la", "description": "List files"})),
            [(Channel::Command, "ls -la".into(), true), (Channel::Payload, "List files".into(), true)]
        );
        assert_eq!(
            fields("edit", json!({"filePath": "/a/b.rs", "oldString": "x", "newString": "y"})),
            [(Channel::Path, "/a/b.rs".into(), true), (Channel::Payload, "y".into(), true)]
        );
        assert_eq!(fields("webfetch", json!({"url": "https://x.dev", "format": "markdown"}))[0], (Channel::Url, "https://x.dev".into(), true));
    }

    #[test]
    fn argv_lists_and_patches() {
        assert_eq!(fields("exec_command", json!({"cmd": ["bash", "-lc", "echo hi"]}))[0].1, "bash -lc echo hi");
        let patch = "*** Begin Patch\n*** Update File: src/a.py\n@@\n-x\n+y\n*** End Patch";
        let found = fields("apply_patch", json!({"input": patch}));
        assert_eq!(found[0].0, Channel::Payload);
        assert_eq!(found[1], (Channel::Path, "src/a.py".into(), true));
    }

    #[test]
    fn required_channels_and_unknowns() {
        assert_eq!(fields("bash", json!({"timeout": 5})), [(Channel::Command, String::new(), false)]);
        assert_eq!(fields("write", json!({"path": "/x"}))[1], (Channel::Payload, String::new(), false));
        assert_eq!(fields("todowrite", json!({"todos": []})), []);
    }
}
