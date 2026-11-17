//! Outbound request facts read from what the agent wrote: literal `curl`/`wget` commands,
//! straight-line inline Python calling `requests`/`httpx`/`urllib.request`, and native
//! fetch tools. Port of atif-scan's `data/outbound_*.py`. Anything dynamic (a variable, a
//! substitution, control flow) yields no request: nothing is ever evaluated.

use std::sync::LazyLock;

use ruff_python_ast::{Expr, ModModule, Stmt};
use serde_json::Value;

use crate::lazy_re;
use crate::model::{Call, Channel, Locator, Trace};
use crate::re::{Lazy, Re, full};
use crate::{net, pysrc, shell};

pub const MAX_CHARS: usize = 65_536;
pub const MAX_ITEMS: usize = 128;
const MAX_DEPTH: usize = 8;
const METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Destination {
    External,
    Local,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Header,
    Body,
    Query,
}

#[derive(Debug, Clone)]
pub struct Carrier {
    pub role: Role,
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct Request {
    pub at: Locator,
    pub target: String,
    pub method: String,
    pub carriers: Vec<Carrier>,
    pub destination: Destination,
}

/// Why a construct isn't read (Python raises `ValueError`).
#[derive(Debug)]
pub struct Unsupported;
type Result<T> = std::result::Result<T, Unsupported>;

/// A literal value: what Python's `ast` constants and containers hold.
#[derive(Debug, Clone)]
pub enum Lit {
    Str(String),
    Bytes(Vec<u8>),
    /// Numbers, booleans, `None`.
    Scalar,
    Dict(Vec<(String, Lit)>),
    List(Vec<Lit>),
}

impl Lit {
    fn from_json(value: &Value) -> Lit {
        match value {
            Value::String(s) => Lit::Str(s.clone()),
            Value::Object(map) => Lit::Dict(map.iter().map(|(k, v)| (k.clone(), Lit::from_json(v))).collect()),
            Value::Array(items) => Lit::List(items.iter().map(Lit::from_json).collect()),
            _ => Lit::Scalar,
        }
    }
}

static DYNAMIC: Lazy = lazy_re!(r"[$`]");

/// Literal text, refusing anything a shell or program would expand.
pub fn scalar(value: &Lit) -> Result<String> {
    match value {
        Lit::Str(s) if s.chars().count() <= MAX_CHARS && !DYNAMIC.is_match(s) => Ok(s.clone()),
        Lit::Bytes(b) => scalar(&Lit::Str(String::from_utf8(b.clone()).map_err(|_| Unsupported)?)),
        _ => Err(Unsupported),
    }
}

fn str_scalar(value: &str) -> Result<String> {
    scalar(&Lit::Str(value.to_string()))
}

/// `urllib.parse.urlsplit`: (scheme, netloc, path, query).
fn urlsplit(target: &str) -> Option<(String, String, String, String)> {
    let colon = target.find(':')?;
    let scheme = &target[..colon];
    let valid = scheme.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c));
    if !valid {
        return None;
    }
    let mut rest = &target[colon + 1..];
    let mut netloc = "";
    if let Some(after) = rest.strip_prefix("//") {
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        netloc = &after[..end];
        rest = &after[end..];
    }
    let rest = rest.split('#').next().unwrap_or("");
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    Some((scheme.to_lowercase(), netloc.to_string(), path.to_string(), query.to_string()))
}

/// `urlsplit(target).hostname`, whether userinfo is present, and `.port` (`Err` when invalid).
fn host_parts(netloc: &str) -> (String, bool, std::result::Result<Option<u32>, ()>) {
    let (userinfo, hostinfo) = match netloc.rsplit_once('@') {
        Some((_, rest)) => (true, rest),
        None => (false, netloc),
    };
    let (host, port) = match hostinfo.split_once('[') {
        Some((_, bracketed)) => {
            let (host, after) = bracketed.split_once(']').unwrap_or((bracketed, ""));
            (host, after.split_once(':').map_or("", |(_, p)| p))
        }
        None => hostinfo.split_once(':').unwrap_or((hostinfo, "")),
    };
    let port = if port.is_empty() {
        Ok(None)
    } else if port.bytes().all(|b| b.is_ascii_digit()) {
        port.parse::<u32>().ok().filter(|p| *p <= 65535).map(Some).ok_or(())
    } else {
        Err(())
    };
    (host.to_lowercase(), userinfo, port)
}

pub fn hostname(target: &str) -> String {
    urlsplit(target).map(|(_, netloc, _, _)| host_parts(&netloc).0).unwrap_or_default()
}

static NUMERIC_HOST: LazyLock<Re> = LazyLock::new(|| full(r"[\d.xa-f]+"));
static UNSAFE_TARGET: Lazy = lazy_re!(r"[\s\\$`{}]");

fn address_destination(host: &str) -> Destination {
    match net::parse(host) {
        Some(ip) => {
            if net::is_global(ip) {
                Destination::External
            } else {
                Destination::Local
            }
        }
        None => {
            if host == "localhost" || host == "localhost.localdomain" || !host.contains('.') {
                return Destination::Local;
            }
            if [".localhost", ".local", ".internal", ".lan", ".home"].iter().any(|s| host.ends_with(s)) {
                return Destination::Local;
            }
            if NUMERIC_HOST.is_match(host) { Destination::Unknown } else { Destination::External }
        }
    }
}

/// No resolution: private literals and local names are not external transfers.
pub fn destination(target: &str) -> Destination {
    let Some((scheme, netloc, _, _)) = urlsplit(target) else { return Destination::Unknown };
    let (host, userinfo, port) = host_parts(&netloc);
    let host = host.trim_end_matches('.').to_string();
    let Ok(port) = port else { return Destination::Unknown };
    let valid = (scheme == "http" || scheme == "https")
        && !host.is_empty()
        && !userinfo
        && port.is_none_or(|p| p > 0 && (p as usize) < MAX_CHARS)
        && !UNSAFE_TARGET.is_match(target);
    if valid { address_destination(&host) } else { Destination::Unknown }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.replace('+', " ").into_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len() + 1
            && let Some(hex) = bytes.get(i + 1..i + 3)
            && let Ok(v) = u8::from_str_radix(std::str::from_utf8(hex).unwrap_or("zz"), 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `parse_qsl(text, max_num_fields=MAX_ITEMS)`: blank values dropped.
fn parse_qsl(text: &str) -> Result<Vec<(String, String)>> {
    let fields: Vec<&str> = text.split('&').collect();
    if fields.len() > MAX_ITEMS {
        return Err(Unsupported);
    }
    Ok(fields
        .into_iter()
        .filter(|f| !f.is_empty())
        .filter_map(|f| f.split_once('='))
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| (percent_decode(k), percent_decode(v)))
        .collect())
}

fn mapping_pairs(items: &[(String, Lit)], role: Role, depth: usize) -> Result<Vec<Carrier>> {
    if items.len() > MAX_ITEMS {
        return Err(Unsupported);
    }
    let mut out = Vec::new();
    for (name, item) in items {
        match item {
            Lit::Dict(_) | Lit::List(_) => out.extend(pairs(item, role, depth + 1)?),
            Lit::Str(_) | Lit::Bytes(_) => out.push(Carrier { role, name: name.clone(), value: scalar(item)? }),
            Lit::Scalar => {}
        }
    }
    Ok(out)
}

fn pairs(value: &Lit, role: Role, depth: usize) -> Result<Vec<Carrier>> {
    if depth > MAX_DEPTH {
        return Err(Unsupported);
    }
    match value {
        Lit::Dict(items) => mapping_pairs(items, role, depth),
        Lit::List(items) if items.len() <= MAX_ITEMS => {
            let mut out = Vec::new();
            for item in items {
                out.extend(pairs(item, role, depth + 1)?);
            }
            Ok(out)
        }
        _ => Err(Unsupported),
    }
}

fn body(value: &Lit) -> Result<Vec<Carrier>> {
    if matches!(value, Lit::Dict(_) | Lit::List(_)) {
        return pairs(value, Role::Body, 0);
    }
    let text = scalar(value)?;
    if text.starts_with('@') {
        return Err(Unsupported);
    }
    let raw = Carrier { role: Role::Body, name: String::new(), value: text.clone() };
    if let Ok(decoded @ (Value::Object(_) | Value::Array(_))) = serde_json::from_str::<Value>(&text) {
        let mut out = vec![raw];
        out.extend(pairs(&Lit::from_json(&decoded), Role::Body, 0)?);
        return Ok(out);
    }
    if text.contains('=') && !text.contains(['\n', '{', '}', '<', '>']) {
        let mut out = vec![raw];
        out.extend(parse_qsl(&text)?.into_iter().map(|(k, v)| Carrier { role: Role::Body, name: k, value: v }));
        return Ok(out);
    }
    Ok(vec![raw])
}

fn request(at: Locator, target: &Lit, method: &str, mut carriers: Vec<Carrier>) -> Result<Request> {
    let target = scalar(target)?;
    if !METHODS.contains(&method) || carriers.len() > MAX_ITEMS {
        return Err(Unsupported);
    }
    let destination = destination(&target);
    if destination == Destination::Unknown {
        return Err(Unsupported);
    }
    let query = urlsplit(&target).map(|(_, _, _, q)| q).unwrap_or_default();
    carriers.extend(parse_qsl(&query)?.into_iter().map(|(k, v)| Carrier { role: Role::Query, name: k, value: v }));
    Ok(Request { at, target, method: method.to_string(), carriers, destination })
}

// --- native fetch tools ---------------------------------------------------------------

const NATIVE: &[&str] = &["http", "webfetch", "web_fetch", "WebFetch", "fetch_url"];
const NATIVE_KEYS: &[&str] = &["url", "method", "headers", "body", "data", "json", "params", "prompt", "format"];

fn native_request(call: &Call, at: Locator) -> Option<Request> {
    let args = call.arguments.as_ref()?;
    if !NATIVE.contains(&call.name.as_str()) || !args.keys().all(|k| NATIVE_KEYS.contains(&k.as_str())) {
        return None;
    }
    let lit = |key: &str| args.get(key).map(Lit::from_json);
    let target = lit("url").unwrap_or(Lit::Scalar);
    let method = match lit("method") {
        Some(m) => scalar(&m).ok()?.to_uppercase(),
        None => if call.name == "http" { String::new() } else { "GET".into() },
    };
    let mut carriers = Vec::new();
    for (key, role) in [("headers", Role::Header), ("params", Role::Query)] {
        if let Some(value) = args.get(key) {
            let Value::Object(map) = value else { return None };
            if !map.values().all(Value::is_string) {
                return None;
            }
            carriers.extend(pairs(&Lit::from_json(value), role, 0).ok()?);
        }
    }
    let payloads: Vec<&Value> = ["body", "data", "json"].iter().filter_map(|k| args.get(*k)).collect();
    if payloads.len() > 1 || (!payloads.is_empty() && !args.contains_key("method")) {
        return None;
    }
    if let Some(payload) = payloads.first() {
        carriers.extend(body(&Lit::from_json(payload)).ok()?);
    }
    request(at, &target, &method, carriers).ok()
}

// --- shell ------------------------------------------------------------------------------

const PYTHON: &[&str] = &["python", "python3", "python2"];
const BINDING: &[&str] = &["alias", "unalias", "eval", "source", ".", "function", "exec"];
const SHELL_KEYWORDS: &[&str] = &["!", "{", "}", "then", "do", "else", "elif", "if", "while", "until", "time", "exec"];
static PYTHON_HTTP: Lazy = lazy_re!(r"\b(?:requests|httpx|urllib\.request)\b");
static COMMENT: Lazy = lazy_re!(r"#[^\n]*");
static QUOTED_HEREDOC: LazyLock<Re> = LazyLock::new(|| full(r#"<<\s*(['"]).+\1"#));
const NO_VALUE: &[&str] = &["-s", "-S", "-sS", "-q", "-f", "-i", "-v", "--silent", "--show-error"];
const CURL_OPTIONS: &[&str] = &["-X", "--request", "-H", "--header", "-d", "--data", "--data-raw", "--data-binary", "--data-urlencode", "--json", "--url", "-o", "--output"];
const WGET_OPTIONS: &[&str] = &["--method", "--header", "--post-data", "--body-data", "-O", "--output-document"];

fn option_kind(name: &str) -> &'static str {
    match name {
        "-X" | "--request" | "--method" => "method",
        "-H" | "--header" => "header",
        "-d" | "--data" | "--data-raw" | "--data-binary" | "--data-urlencode" | "--json" | "--post-data" | "--body-data" => "body",
        "--url" => "target",
        _ => "ignore",
    }
}

fn scope_supported(script: &shell::Script, text: &str) -> bool {
    if !script.complete || script.order.len() > MAX_ITEMS {
        return false;
    }
    for command in script.commands() {
        let Some(first) = command.words.first() else { return false };
        if BINDING.contains(&first.as_str()) || SHELL_KEYWORDS.contains(&first.as_str()) || shell::is_assignment(first) || command.background || command.repeated || !command.nested.is_empty() || !command.substituted.is_empty() {
            return false;
        }
    }
    let mut spans: Vec<(usize, usize)> = script.commands().flat_map(|c| c.spans.iter().copied().chain(c.redirects.iter().map(|r| r.2))).collect();
    for &(start, end) in &script.bodies {
        let closing = text[end..].find('\n').map_or(text.len(), |n| end + n + 1);
        spans.push((start, closing));
    }
    let mut blanked = text.as_bytes().to_vec();
    let len = blanked.len();
    for (start, end) in spans {
        blanked[start.min(len)..end.min(len)].fill(b' ');
    }
    let blanked = String::from_utf8_lossy(&blanked);
    let mut residue = String::new();
    let mut last = 0;
    for (s, e) in COMMENT.iter(&blanked) {
        residue.push_str(&blanked[last..s]);
        last = e;
    }
    residue.push_str(&blanked[last..]);
    residue.trim_matches([' ', '\t', '\r', '\n', ';']).is_empty()
}

/// One option of a curl/wget command: (kind, value, next index).
fn option(words: &[String], index: usize) -> Result<(&'static str, String, usize)> {
    let word = &words[index];
    let (name, sep, value) = if word.starts_with("--") {
        match word.split_once('=') {
            Some((n, v)) => (n, true, v.to_string()),
            None => (word.as_str(), false, String::new()),
        }
    } else {
        (word.as_str(), false, String::new())
    };
    if NO_VALUE.contains(&name) && (words[0] == "curl" || ["-q", "-S", "-v"].contains(&name)) {
        return Ok(("ignore", String::new(), index + 1));
    }
    if words[0] == "curl" && ["-I", "--head", "-O"].contains(&name) {
        return Ok(if name == "-O" { ("ignore", String::new(), index + 1) } else { ("method", "HEAD".into(), index + 1) });
    }
    let allowed = if words[0] == "curl" { CURL_OPTIONS } else { WGET_OPTIONS };
    if !allowed.contains(&name) {
        return Err(Unsupported);
    }
    let value = if sep {
        value
    } else {
        words.get(index + 1).cloned().ok_or(Unsupported)?
    };
    if name == "--data-urlencode" && value.contains('@') && !value.split('@').next().unwrap_or("").contains('=') {
        return Err(Unsupported);
    }
    Ok((option_kind(name), str_scalar(&value)?, index + if sep { 1 } else { 2 }))
}

fn header(value: &str) -> Result<Carrier> {
    let (name, content) = value.split_once(':').ok_or(Unsupported)?;
    if name.trim().is_empty() || value.starts_with('@') {
        return Err(Unsupported);
    }
    Ok(Carrier { role: Role::Header, name: name.trim().to_string(), value: content.trim().to_string() })
}

fn http_command(command: &shell::Command, at: Locator) -> Result<Option<Request>> {
    if !command.redirects.is_empty() || command.words.len() > MAX_ITEMS {
        return Ok(None);
    }
    let (mut targets, mut methods, mut headers, mut bodies) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut index = 1;
    while index < command.words.len() {
        let (kind, value, next) = if command.words[index].starts_with('-') { option(&command.words, index)? } else { ("target", str_scalar(&command.words[index])?, index + 1) };
        match kind {
            "target" => targets.push(value),
            "method" => methods.push(value),
            "header" => headers.push(value),
            "body" => bodies.push(value),
            _ => {}
        }
        index = next;
    }
    if targets.len() != 1 || methods.len() > 1 {
        return Err(Unsupported);
    }
    let method = methods.first().map(|m| m.to_uppercase()).unwrap_or_else(|| if bodies.is_empty() { "GET".into() } else { "POST".into() });
    let mut carriers = headers.iter().map(|h| header(h)).collect::<Result<Vec<_>>>()?;
    for value in &bodies {
        carriers.extend(body(&Lit::Str(value.clone()))?);
    }
    request(at, &Lit::Str(targets.remove(0)), &method, carriers).map(Some)
}

fn lookup(command: &shell::Command) -> bool {
    command.words.len() >= 2 && command.words[0] == "command" && (command.words[1] == "-v" || command.words[1] == "-V")
}

fn python_candidate(id: shell::Id, command: &shell::Command, script: &shell::Script, text: &str) -> bool {
    if !command.name().is_some_and(|n| PYTHON.contains(&n)) {
        return false;
    }
    let words = command.words[1..].join(" ");
    if PYTHON_HTTP.is_match(&words) || words.contains('$') || words.contains('`') {
        return true;
    }
    script.bodies.iter().zip(&script.body_commands).any(|(&(a, b), &owner)| owner == id && PYTHON_HTTP.is_match(&text[a..b]))
}

fn python_command(id: shell::Id, command: &shell::Command, script: &shell::Script, text: &str, at: Locator) -> Result<Vec<Request>> {
    let args = &command.words[1..];
    if args.len() == 2 && args[0] == "-c" && command.redirects.is_empty() {
        return python_requests(&str_scalar(&args[1])?, at);
    }
    let bodies: Vec<(usize, usize)> = script.bodies.iter().zip(&script.body_commands).filter(|(_, owner)| **owner == id).map(|(s, _)| *s).collect();
    if (args.is_empty() || args == ["-"]) && bodies.len() == 1 && command.redirects.len() == 1 {
        let (op, _, (start, end)) = &command.redirects[0];
        if op == "<<" && QUOTED_HEREDOC.is_match(&text[*start..*end]) {
            let (a, b) = bodies[0];
            return python_requests(&text[a..b], at);
        }
    }
    Ok(Vec::new())
}

fn candidate(script: &shell::Script, text: &str) -> bool {
    script.commands_with_ids().any(|(id, c)| {
        !lookup(c) && (c.name().is_some_and(|n| n == "curl" || n == "wget") || c.words.first().is_some_and(|w| w.contains('$')) || python_candidate(id, c, script, text))
    })
}

/// The requests a shell command makes.
pub fn shell_requests(text: &str, at: Locator) -> Vec<Request> {
    if text.chars().count() > MAX_CHARS {
        return Vec::new();
    }
    let script = shell::parse(text);
    if script.complete && !candidate(&script, text) {
        return Vec::new();
    }
    if !scope_supported(&script, text) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (id, command) in script.commands_with_ids() {
        let place = at.with_span(command.span);
        let parsed = if command.name().is_some_and(|n| n == "curl" || n == "wget") && !lookup(command) {
            http_command(command, place).map(|r| r.into_iter().collect())
        } else if python_candidate(id, command, &script, text) {
            python_command(id, command, &script, text, place)
        } else {
            Ok(Vec::new())
        };
        if let Ok(found) = parsed {
            out.extend(found);
        }
    }
    out
}

// --- inline Python ------------------------------------------------------------------------

const MODULES: &[&str] = &["requests", "httpx", "urllib.request"];
const HTTP_OPERATIONS: &[&str] = &["get", "post", "put", "patch", "delete", "head", "options", "request"];
const OPTIONS: &[&str] = &["headers", "data", "json", "params", "timeout"];

fn literal(node: &Expr, depth: usize) -> Result<Lit> {
    if depth > MAX_DEPTH {
        return Err(Unsupported);
    }
    match node {
        Expr::StringLiteral(s) => Ok(Lit::Str(s.value.to_str().to_string())),
        Expr::BytesLiteral(b) => Ok(Lit::Bytes(b.value.bytes().collect())),
        Expr::NumberLiteral(_) | Expr::BooleanLiteral(_) | Expr::NoneLiteral(_) | Expr::EllipsisLiteral(_) => Ok(Lit::Scalar),
        Expr::Dict(dict) if dict.items.len() <= MAX_ITEMS => {
            let mut out = Vec::new();
            for item in &dict.items {
                let key = item.key.as_ref().ok_or(Unsupported)?;
                out.push((scalar(&literal(key, depth + 1)?)?, literal(&item.value, depth + 1)?));
            }
            Ok(Lit::Dict(out))
        }
        Expr::List(list) if list.elts.len() <= MAX_ITEMS => Ok(Lit::List(list.elts.iter().map(|e| literal(e, depth + 1)).collect::<Result<_>>()?)),
        Expr::Tuple(tuple) if tuple.elts.len() <= MAX_ITEMS => Ok(Lit::List(tuple.elts.iter().map(|e| literal(e, depth + 1)).collect::<Result<_>>()?)),
        _ => Err(Unsupported),
    }
}

fn dotted(node: &Expr) -> String {
    match node {
        Expr::Name(name) => name.id.to_string(),
        Expr::Attribute(attr) => format!("{}.{}", dotted(&attr.value), attr.attr),
        _ => String::new(),
    }
}

fn http_call(call: &ruff_python_ast::ExprCall, at: Locator, bindings: &[String]) -> Result<Request> {
    let name = dotted(&call.func);
    let (module, operation) = name.rsplit_once('.').unwrap_or(("", &name));
    if !bindings.iter().any(|b| b == module) {
        return Err(Unsupported);
    }
    if (module == "urllib.request" && operation != "urlopen") || (module != "urllib.request" && !HTTP_OPERATIONS.contains(&operation)) {
        return Err(Unsupported);
    }
    let mut args: Vec<&Expr> = call.arguments.args.iter().collect();
    let keywords = &call.arguments.keywords;
    let method = if operation == "request" && (module == "requests" || module == "httpx") && !args.is_empty() {
        scalar(&literal(args.remove(0), 0)?)?.to_uppercase()
    } else if module == "urllib.request" && operation == "urlopen" {
        if args.len() > 1 || keywords.iter().any(|k| k.arg.as_ref().is_some_and(|a| a.as_str() == "data")) { "POST".into() } else { "GET".into() }
    } else {
        operation.to_uppercase()
    };
    if args.is_empty() || args.len() > if operation == "urlopen" { 2 } else { 1 } {
        return Err(Unsupported);
    }
    let target = literal(args[0], 0)?;
    let mut options: Vec<(String, Lit)> = Vec::new();
    for keyword in keywords {
        let Some(arg) = &keyword.arg else { return Err(Unsupported) };
        let arg = arg.as_str();
        if !OPTIONS.contains(&arg) || options.iter().any(|(k, _)| k == arg) {
            return Err(Unsupported);
        }
        options.push((arg.to_string(), literal(&keyword.value, 0)?));
    }
    if args.len() == 2 {
        options.push(("data".into(), literal(args[1], 0)?));
    }
    let mut carriers = Vec::new();
    for (name, value) in &options {
        match name.as_str() {
            "headers" | "params" => {
                let Lit::Dict(items) = value else { return Err(Unsupported) };
                if !items.iter().all(|(_, v)| matches!(v, Lit::Str(_))) {
                    return Err(Unsupported);
                }
                carriers.extend(pairs(value, if name == "headers" { Role::Header } else { Role::Query }, 0)?);
            }
            "data" | "json" => carriers.extend(body(value)?),
            _ => {}
        }
    }
    request(at, &target, &method, carriers)
}

fn bind_import(names: &[ruff_python_ast::Alias], bindings: &mut Vec<String>) -> bool {
    let mut complete = true;
    for alias in names {
        let full_name = alias.name.as_str();
        let root = alias.asname.as_ref().map_or(full_name, |a| a.as_str()).split('.').next().unwrap_or("").to_string();
        bindings.retain(|m| m.split('.').next() != Some(root.as_str()));
        if MODULES.contains(&full_name) && alias.asname.is_none() {
            bindings.push(full_name.to_string());
        } else {
            complete = false;
        }
    }
    complete
}

fn module_requests(module: &ModModule, at: Locator) -> Vec<Request> {
    let mut bindings: Vec<String> = Vec::new();
    let mut facts = Vec::new();
    for statement in module.body.iter().take(MAX_ITEMS) {
        match statement {
            Stmt::Import(import) => {
                bind_import(&import.names, &mut bindings);
            }
            // A docstring or other bare constant.
            Stmt::Expr(e) if matches!(&*e.value, Expr::StringLiteral(_) | Expr::BytesLiteral(_) | Expr::NumberLiteral(_) | Expr::BooleanLiteral(_) | Expr::NoneLiteral(_) | Expr::EllipsisLiteral(_)) => continue,
            _ => {
                let call = match statement {
                    Stmt::Expr(e) => e.value.as_call_expr(),
                    Stmt::Assign(a) => a.value.as_call_expr(),
                    Stmt::AnnAssign(a) => a.value.as_deref().and_then(Expr::as_call_expr),
                    _ => None,
                };
                let Some(call) = call else { break };
                match http_call(call, at, &bindings) {
                    Ok(fact) => facts.push(fact),
                    Err(_) => break,
                }
                // Even a request result may rebind requests or an attribute on it.
                if !matches!(statement, Stmt::Expr(_)) {
                    bindings.clear();
                }
            }
        }
    }
    facts
}

/// Literal HTTP calls in straight-line Python.
pub fn python_requests(source: &str, at: Locator) -> Result<Vec<Request>> {
    if source.chars().count() > MAX_CHARS {
        return Ok(Vec::new());
    }
    let Some(module) = pysrc::parse_module(source) else { return Ok(Vec::new()) };
    if pysrc::node_count(&module) > MAX_ITEMS * MAX_DEPTH {
        return Ok(Vec::new());
    }
    Ok(module_requests(&module, at))
}

/// Every request the agent's calls make, in trace order.
pub fn outbound_requests(trace: &Trace) -> Vec<Request> {
    let mut facts = Vec::new();
    for (step, call) in trace.agent_calls() {
        if call.tool != "shell" && call.tool != "web_fetch" && !NATIVE.contains(&call.name.as_str()) {
            continue;
        }
        let at = Locator { call: Some(call.index), ..Locator::step(step.index, Channel::Arguments) };
        if call.tool != "shell" {
            facts.extend(native_request(call, at));
        } else {
            for (index, (channel, content)) in call.fields.iter().enumerate() {
                if *channel == Channel::Command && content.understood {
                    let place = Locator { call: Some(call.index), field: Some(index), ..Locator::step(step.index, Channel::Command) };
                    facts.extend(shell_requests(&content.text, place));
                }
            }
        }
        if facts.len() > MAX_ITEMS {
            facts.truncate(MAX_ITEMS);
            break;
        }
    }
    facts
}
