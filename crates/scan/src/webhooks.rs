//! Bearer webhook URLs (Slack, Discord): complete capability targets, written directly, as
//! an escaped JSON string, or assembled from Python string literals with `+`. Port of
//! atif-scan's `data/webhooks.py`. Shape recognition only, never proof of issuance or use.

use std::collections::HashMap;
use std::sync::LazyLock;

use ruff_python_ast::{Expr, ModModule, Operator, Stmt};

use crate::lazy_re;
use crate::pysrc;
use crate::re::{Lazy, Re, Span, full};

const SLACK_HOSTS: &[&str] = &["hooks.slack.com", "hooks.slack-gov.com"];
const DISCORD_HOSTS: &[&str] = &["discord.com", "discordapp.com", "canary.discord.com", "ptb.discord.com", "canary.discordapp.com", "ptb.discordapp.com"];
static SLACK_PATH: LazyLock<Re> = LazyLock::new(|| full(r"/services/(?P<first>T[A-Z0-9]{8,12})/(?P<second>B[A-Z0-9]{8,12})/(?P<token>[A-Za-z0-9]{24,64})"));
static DISCORD_PATH: LazyLock<Re> = LazyLock::new(|| full(r"/api(?:/v[0-9]{1,2})?/webhooks/(?P<first>[0-9]{17,20})/(?P<token>[A-Za-z0-9_-]{60,100})(?:/(?:slack|github))?"));
static URL: Lazy = lazy_re!(r#"(?i)(?<![\w:/])https?://[^\s'"<>`\\]+"#);
static JSON_STRING: Lazy = lazy_re!(r#""(?:[^"\\\x00-\x1f]|\\(?:["\\/bfnrt]|u[0-9a-fA-F]{4})){1,4096}""#);
const MAX_SOURCE: usize = 65_536;
const MAX_NODES: usize = 1024;
const MAX_DEPTH: usize = 20;
const MAX_PARTS: usize = 64;
const MAX_VALUE: usize = 8192;

/// A structurally valid capability.
#[derive(Debug, Clone)]
pub struct Capability {
    pub service: &'static str,
    pub token: String,
    /// (component, span in the URL).
    components: Vec<(String, Span)>,
}

/// `urllib.parse.urlsplit` enough for an `https://host/path` target: (scheme, netloc, path).
pub fn split(url: &str) -> Option<(String, &str, &str)> {
    let colon = url.find(':')?;
    let scheme = &url[..colon];
    let valid = scheme.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c));
    if !valid {
        return None;
    }
    let rest = &url[colon + 1..];
    let (netloc, rest) = match rest.strip_prefix("//") {
        Some(after) => {
            let end = after.find(['/', '?', '#']).unwrap_or(after.len());
            (&after[..end], &after[end..])
        }
        None => ("", rest),
    };
    let path_end = rest.find(['?', '#']).unwrap_or(rest.len());
    Some((scheme.to_lowercase(), netloc, &rest[..path_end]))
}

/// Decode only escaped unreserved bytes, keeping each decoded character's raw span.
fn decode_path(path: &str) -> Option<(String, Vec<Span>)> {
    let bytes = path.as_bytes();
    let (mut decoded, mut spans) = (String::new(), Vec::new());
    let mut pos = 0;
    while pos < path.len() {
        let (c, end) = if bytes[pos] == b'%' {
            let escape = path.get(pos + 1..pos + 3)?;
            if escape.len() != 2 || !escape.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            let c = u8::from_str_radix(escape, 16).ok()? as char;
            if !(c.is_ascii() && (c.is_ascii_alphanumeric() || "-._~".contains(c))) {
                return None;
            }
            (c, pos + 3)
        } else {
            let c = path[pos..].chars().next()?;
            (c, pos + c.len_utf8())
        };
        decoded.push(c);
        spans.push((pos, end));
        pos = end;
    }
    Some((decoded, spans))
}

fn target(url: &str) -> Option<Capability> {
    if url.chars().any(|c| c.is_whitespace() || (c as u32) < 32 || c as u32 == 127 || c == '\\') {
        return None;
    }
    let (scheme, netloc, path) = split(url)?;
    let mut host = netloc.to_lowercase();
    if host.ends_with(":443") {
        host.truncate(host.len() - 4);
    }
    if scheme != "https" || !(SLACK_HOSTS.contains(&host.as_str()) || DISCORD_HOSTS.contains(&host.as_str())) {
        return None;
    }
    let (decoded, spans) = decode_path(path)?;
    let (service, pattern, names): (_, &Re, &[&str]) =
        if SLACK_HOSTS.contains(&host.as_str()) { ("slack", &SLACK_PATH, &["first", "second", "token"]) } else { ("discord", &DISCORD_PATH, &["first", "token"]) };
    let found = pattern.captures(&decoded)?;
    // The decoded path is ASCII when it matches, so its byte offsets index `spans`.
    let offset = url[url.find("://")? + 3..].find(path).map(|i| i + url.find("://").unwrap() + 3)?;
    let components = names
        .iter()
        .map(|name| {
            let m = found.name(name).unwrap();
            (m.as_str().to_string(), (offset + spans[m.start()].0, offset + spans[m.end() - 1].1))
        })
        .collect();
    Some(Capability { service, token: found.name("token").unwrap().as_str().to_string(), components })
}

/// Recognize one complete URL.
pub fn webhook_capability(url: &str) -> Option<Capability> {
    target(url)
}

/// Documentation's XXXXXX/000000 stand-ins are masked, but not findings.
fn exposable(token: &str) -> bool {
    let lower = token.to_lowercase();
    let mut distinct: Vec<char> = lower.chars().collect();
    distinct.sort();
    distinct.dedup();
    distinct.len() > 1 && token.chars().any(char::is_alphanumeric)
}

/// (whole URL, component, value) for each capability written directly.
fn direct(text: &str, exposure: bool) -> Vec<(Span, Span, String)> {
    let mut out = Vec::new();
    for (start, end) in URL.iter(text) {
        let url = text[start..end].trim_end_matches(['.', ',', ';', ':', '!', ')', ']', '}']);
        let Some(capability) = target(url) else { continue };
        if exposure && !exposable(&capability.token) {
            continue;
        }
        let whole = (start, start + url.len());
        for (value, (s, e)) in capability.components {
            out.push((whole, (start + s, start + e), value));
        }
    }
    out
}

#[derive(Clone)]
struct Piece {
    value: String,
    source: Span,
}

fn read(node: &Expr, env: &HashMap<String, Vec<Piece>>, depth: usize) -> Vec<Piece> {
    if depth > MAX_DEPTH {
        return Vec::new();
    }
    match node {
        Expr::StringLiteral(literal) => {
            let value = literal.value.to_str();
            if value.chars().count() <= MAX_VALUE { vec![Piece { value: value.to_string(), source: pysrc::span(literal) }] } else { Vec::new() }
        }
        Expr::Name(name) => env.get(name.id.as_str()).cloned().unwrap_or_default(),
        Expr::BinOp(binop) if binop.op == Operator::Add => {
            let left = read(&binop.left, env, depth + 1);
            let right = read(&binop.right, env, depth + 1);
            if left.is_empty() || right.is_empty() {
                return Vec::new();
            }
            let parts: Vec<Piece> = left.into_iter().chain(right).collect();
            if parts.len() > MAX_PARTS || parts.iter().map(|p| p.value.chars().count()).sum::<usize>() > MAX_VALUE {
                return Vec::new();
            }
            parts
        }
        _ => Vec::new(),
    }
}

fn tree(text: &str) -> Option<ModModule> {
    if text.chars().count() > MAX_SOURCE || !text.contains('+') {
        return None;
    }
    let module = pysrc::parse_module(text)?;
    (pysrc::node_count(&module) <= MAX_NODES).then_some(module)
}

/// Literal parts joined with `+` at top level, reading uniquely bound earlier names.
fn assemblies(text: &str) -> Vec<Vec<Piece>> {
    let Some(module) = tree(text) else { return Vec::new() };
    let mut stores: HashMap<String, usize> = HashMap::new();
    for name in pysrc::bound_names(&module) {
        *stores.entry(name).or_default() += 1;
    }
    let mut env: HashMap<String, Vec<Piece>> = HashMap::new();
    let mut out = Vec::new();
    for statement in &module.body {
        if !matches!(statement, Stmt::Assign(_) | Stmt::AnnAssign(_) | Stmt::Expr(_)) {
            continue;
        }
        for expr in pysrc::expressions(statement) {
            if matches!(expr, Expr::BinOp(_)) {
                let parts = read(expr, &env, 0);
                if !parts.is_empty() {
                    out.push(parts);
                }
            }
        }
        if let Stmt::Assign(assign) = statement
            && let [Expr::Name(name)] = assign.targets.as_slice()
            && stores.get(name.id.as_str()) == Some(&1)
        {
            env.insert(name.id.to_string(), read(&assign.value, &env, 0));
        }
    }
    out
}

/// Literal pieces of an assembled capability.
fn contributions(text: &str, exposure: bool) -> Vec<Span> {
    let mut out = Vec::new();
    for parts in assemblies(text) {
        let joined: String = parts.iter().map(|p| p.value.as_str()).collect();
        for (_, (start, end), _) in direct(&joined, exposure) {
            let mut offset = 0;
            for part in &parts {
                let next = offset + part.value.len();
                if offset < end && start < next {
                    out.push(part.source);
                }
                offset = next;
            }
        }
    }
    out
}

fn json_capabilities(text: &str, exposure: bool) -> Vec<Span> {
    if !text.contains(r"\/") {
        return Vec::new();
    }
    JSON_STRING
        .iter(text)
        .filter(|(s, e)| text[*s..*e].contains(r"\/"))
        .filter(|(s, e)| {
            serde_json::from_str::<String>(&text[*s..*e])
                .ok()
                .and_then(|decoded| target(&decoded))
                .is_some_and(|c| !exposure || exposable(&c.token))
        })
        .collect()
}

/// (whole locator, sensitive value) spans.
pub fn find(text: &str, exposure: bool) -> Vec<(Span, Span)> {
    let mut out: Vec<(Span, Span)> = direct(text, exposure).into_iter().map(|(whole, value, _)| (whole, value)).collect();
    out.extend(contributions(text, exposure).into_iter().map(|s| (s, s)));
    out.extend(json_capabilities(text, exposure).into_iter().map(|s| (s, s)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLACK: &str = "https://hooks.slack.com/services/T01234567/B01234567/abcdefghijklmnopqrstuvwxABCDEFGHIJKLMNOPQRSTUVWX0123456789ab";

    #[test]
    fn direct_json_and_assembled() {
        assert!(!find(SLACK, true).is_empty());
        // Documentation's all-X stand-in (built here so it isn't a literal webhook URL).
        let placeholder = format!("https://hooks.slack.com/services/T{x8}/B{x8}/{x24}", x8 = "X".repeat(8), x24 = "X".repeat(24));
        assert!(find(&placeholder, true).is_empty());
        let json = SLACK.replace('/', "\\/");
        assert!(!find(&format!(r#"{{"url": "{json}"}}"#), true).is_empty());
        let assembled = "base = 'https://hooks.slack.com/services/'\nurl = base + 'T01234567/B01234567/' + 'abcdefghijklmnopqrstuvwxABCDEFGHIJKLMNOPQRSTUVWX0123456789ab'\n";
        assert!(!find(assembled, true).is_empty());
        assert!(find("https://example.com/services/T01234567/B01234567/abcdefghijklmnopqrstuvwxABCDEFGHIJKLMNOPQRSTUVWX0123456789ab", true).is_empty());
    }
}
