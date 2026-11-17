//! Credential shapes: token shapes (`sk-…`, `hf_…`, `ghp_…`, AWS key IDs, JWTs, private
//! keys, harness keys), bearer webhooks, and values whose *name* says they are secret.
//! Port of atif-scan's `data/credentials.py`. Findings carry spans only.

use std::sync::LazyLock;

use crate::lazy_re;
use crate::re::{Lazy, Re, Span, full};
use crate::webhooks;

const PRIVATE_KEY: &str = r"-----BEGIN [A-Z ]*PRIVATE KEY-----.*?(?:-----END [A-Z ]*PRIVATE KEY-----|$)";
static TOKEN_SHAPES: LazyLock<Re> = LazyLock::new(|| {
    Re::new(&format!(
        r"(?s)(?=[-sprhgxAeL])(?:{PRIVATE_KEY}|\b(?:sk|pk|rk)-(?=[\w-]*\d)[A-Za-z0-9_-]{{16,}}|\bhf_[A-Za-z0-9]{{20,}}|\bgh[pousr]_[A-Za-z0-9]{{20,}}|\bgithub_pat_[A-Za-z0-9_]{{20,}}|\bxox[abprs]-[A-Za-z0-9-]{{10,}}|\bAKIA[0-9A-Z]{{16}}\b|\bAIza[0-9A-Za-z_-]{{35}}|\beyJ[A-Za-z0-9_-]{{10,}}\.[A-Za-z0-9_-]{{10,}}\.[A-Za-z0-9_-]{{10,}}|\bLLM\|\d{{6,}}\|[A-Za-z0-9_-]{{12,}})"
    ))
});
/// NAME=value, the value optionally quoted; not a comparison (`==`).
static ENV: Lazy = lazy_re!(r#"(?<![\w$])(?P<name>[A-Za-z_][A-Za-z0-9_]*)=(?!=)(?P<q>['"]?)(?P<value>[^\s'"]+)"#);
/// 'name': 'value' / name="value": the value must be quoted.
static KEYED: Lazy = lazy_re!(r#"(?<![\w-])(?P<nq>['"]?)(?P<name>[A-Za-z_][\w-]*)\k<nq>\]?\s*[:=]\s*(?P<q>['"])(?P<value>[^'"\s]+)\k<q>"#);
/// --api-key VALUE / x-api-key: VALUE / Authorization: Bearer VALUE.
static FLAG: Lazy = lazy_re!(
    r#"(?i)(?=[-xap])(?:(?<![\w-])--?(?P<flag>api[_-]?key|token|password|secret)[= ]\s*|\b(?P<header>x-api-key|api-key|x-[a-z0-9-]*token)\s*:\s*|\b(?:proxy-)?authorization\s*:\s*bearer\s+)(?P<q>['"]?)(?P<value>[^\s'"]+)"#
);
static AUTHORIZATION: Lazy = lazy_re!(
    r#"(?i)(?<![\w-])(?P<nq>['"]?)(?:proxy-)?authorization\k<nq>\s*[:=]\s*['"]?(?:Bearer|Basic|Token|Key)[ \t]+(?P<value>[^\s'"]{8,})"#
);
static SECRET_PART: Lazy = lazy_re!(r"(?:^|_)(?:api_?key|apikey|secret|token|password|passwd|pwd|credentials?|private_key|access_key|secret_key|session_key|auth_key|key)(?:_|$)");
static NOT_SECRET_PART: Lazy = lazy_re!(
    r"(?:^|_)(?:path|file|dir|url|uri|host|port|name|type|cache|len|length|size|count|max|min|limit|env|header|field|prefix|id|ids|format|mode|var|vars|fn|func|error|source|public|publishable|site|captcha|usage|values?|mask|padding|owner|oid|chars?|gpg|pgp|fingerprint|primary|sort|partition|display|lookup)(?:_|$)"
);
static IDENTIFIER: LazyLock<Re> = LazyLock::new(|| {
    full(r"[a-z]+(?:_[a-z]+)+|[a-z]+(?:[A-Z][a-z]+)+|[a-z_]+(?::[a-z_]+)+|__[a-z_]+__|[a-z_]+(?:\.(?:[a-z_]+|\d+))*\.\d+(?:\.(?:[a-z_]+|\d+))+")
});
static PLACEHOLDER: Lazy = lazy_re!(
    r"(?i)^(?:\*+|x+|\.+|<[^>]*>|\$\{?\w+\}?|%\w+%|your[\w-]*|changeme|none|null|nil|true|false|dummy|test\w*|example\w*|placeholder|redacted|masked|secret|password|undefined|sk-\.\.\.|\.\.\.)$"
);
static CODE_VALUE: LazyLock<Re> = LazyLock::new(|| full(r"[A-Za-z_]\w*(?:\.[A-Za-z_]\w*)+|[A-Z][A-Z0-9]*(?:_[A-Z0-9]+)+(?:=\S*)?"));
static NAME_SPLIT: Lazy = lazy_re!(r"[_\-]+|(?<=[a-z0-9])(?=[A-Z])");
static ACRONYM: Lazy = lazy_re!(r"([A-Z]+)([A-Z][a-z])");
static LITERAL_PLACEHOLDER: LazyLock<Re> = LazyLock::new(|| {
    full(r"(?i)(?:\*+|x+|\.+|<[^>]*>|\$\{?\w+\}?|%\w+%|your[\w-]*|changeme|none|null|nil|true|false|dummy|test|example|placeholder|redacted|masked|secret|password|undefined|sk-\.\.\.)")
});
static DUMMY_CREDENTIAL: LazyLock<Re> = LazyLock::new(|| {
    full(r"(?i)(?:sk[-_])?(?:dummy|test|example|fake|placeholder)[-_](?:(?:api|access|auth|secret)[-_])?(?:key|token|secret|password)(?:[-_](?:\d+|dummy|test|example|fake|placeholder))?")
});
static PEM_PLACEHOLDER: LazyLock<Re> =
    LazyLock::new(|| full(r"-----BEGIN (?P<label>[A-Z ]*PRIVATE KEY)-----\s*(?:\.{3}|…|<[^<>\r\n]*>)\s*-----END \k<label>-----"));
const PUBLIC_SSH: &str = r"(?<![\w@.-])sk-(?:ssh-ed25519|ecdsa-sha2-nistp256)(?:-cert-v01)?@openssh\.com(?![\w@.-])";
static PUBLIC_SSH_ALGORITHM: LazyLock<Re> = LazyLock::new(|| Re::new(PUBLIC_SSH));
static PUBLIC_SSH_FULL: LazyLock<Re> = LazyLock::new(|| full(PUBLIC_SSH));
/// Tokenizer pieces (`Ġ`, `Ċ`, `▁`): no issued key holds these.
static TOKENIZER_PIECE: Lazy = lazy_re!("[\u{0120}\u{010a}\u{2581}]");

const AUTH_PARTS: &[&str] = &["api", "apikey", "secret", "token", "password", "passwd", "pwd", "credential", "credentials", "private", "access", "session", "auth"];
const LOOKUP_PARTS: &[&str] = &["setting", "settings", "enabled", "analysis", "description", "options", "routing", "merge", "foreign", "match", "scope", "plugin", "handle", "map"];
pub const MIN_SECRET_LENGTH: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Token,
    Webhook,
    /// Mask-only (an `Authorization` header value).
    Authorization,
    Named,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Found {
    pub kind: Kind,
    /// Where to point a citation: the name, for named values.
    pub span: Span,
    /// The secret itself.
    pub value: Span,
}

fn name_parts(name: &str) -> Vec<String> {
    NAME_SPLIT.split(name).into_iter().filter(|p| !p.is_empty()).map(str::to_lowercase).collect()
}

/// Does an assignment name say it holds a secret?
pub fn secret_name(name: &str) -> bool {
    let joined = name_parts(name).join("_");
    if joined == "key" {
        return false;
    }
    SECRET_PART.is_match(&joined) && !NOT_SECRET_PART.is_match(&joined)
}

fn is_digits(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|c| c.is_numeric())
}

fn len_chars(value: &str) -> usize {
    value.chars().count()
}

fn plausible_value(value: &str) -> bool {
    if len_chars(value) < MIN_SECRET_LENGTH || PLACEHOLDER.is_match(value) || is_digits(value) {
        return false;
    }
    !code_like(value) && value.chars().any(char::is_alphabetic)
}

/// A path, URL, substitution, code expression or bare identifier.
fn code_like(value: &str) -> bool {
    value.starts_with(['/', '~', '.'])
        || value.contains("://")
        || value.starts_with("$(")
        || value.starts_with('`')
        || value.contains(['(', ')', '[', ']', '{', '}', '<', '>', '\\'])
        || value.ends_with(',')
        || CODE_VALUE.is_match(value)
        || IDENTIFIER.is_match(value)
}

fn exposure_name(name: &str) -> bool {
    if !secret_name(name) {
        return false;
    }
    // Also split acronym-to-word boundaries (APIKey).
    let mut split = String::new();
    let mut last = 0;
    for c in ACRONYM.captures_iter(name) {
        let m = c.get(0).unwrap();
        split.push_str(&name[last..m.start()]);
        split.push_str(&format!("{}_{}", &c[1], &c[2]));
        last = m.end();
    }
    split.push_str(&name[last..]);
    let parts: Vec<String> = NAME_SPLIT.split(&split).into_iter().map(str::to_lowercase).collect();
    if parts.iter().any(|p| AUTH_PARTS.contains(&p.as_str())) {
        return true;
    }
    !parts.iter().any(|p| LOOKUP_PARTS.contains(&p.as_str()))
}

fn exposure_value(value: &str) -> bool {
    if len_chars(value) < MIN_SECRET_LENGTH || is_digits(value) || TOKENIZER_PIECE.is_match(value) {
        return false;
    }
    if LITERAL_PLACEHOLDER.is_match(value) || DUMMY_CREDENTIAL.is_match(value) || PUBLIC_SSH_FULL.is_match(value) {
        return false;
    }
    !code_like(value) && value.chars().any(char::is_alphabetic)
}

fn masked_value(value: &str) -> bool {
    plausible_value(value) || exposure_value(value)
}

fn group_span(c: &crate::re::Captures, name: &str) -> Span {
    let m = c.name(name).unwrap();
    (m.start(), m.end())
}

/// Secret-named assignments and secret flags.
fn named(text: &str, secret: fn(&str) -> bool, plausible: fn(&str) -> bool, out: &mut Vec<Found>) {
    for pattern in [&*ENV, &*KEYED] {
        for c in pattern.captures_iter(text) {
            if secret(c.name("name").unwrap().as_str()) && plausible(c.name("value").unwrap().as_str()) {
                out.push(Found { kind: Kind::Named, span: group_span(&c, "name"), value: group_span(&c, "value") });
            }
        }
    }
    for c in FLAG.captures_iter(text) {
        if plausible(c.name("value").unwrap().as_str()) {
            let m = c.get(0).unwrap();
            out.push(Found { kind: Kind::Named, span: (m.start(), m.end()), value: group_span(&c, "value") });
        }
    }
}

/// A known token shape somewhere in `value`.
pub fn token_shape(value: &str) -> bool {
    TOKEN_SHAPES.is_match(value)
}

/// atif-scan's historical "plausible secret value" heuristic.
pub fn plausible(value: &str) -> bool {
    plausible_value(value)
}

/// Broad masking candidates (privacy, not review findings).
pub fn find(text: &str) -> Vec<Found> {
    let mut out = Vec::new();
    if text.is_empty() {
        return out;
    }
    for span in TOKEN_SHAPES.iter(text) {
        out.push(Found { kind: Kind::Token, span, value: span });
    }
    for (span, value) in webhooks::find(text, false) {
        out.push(Found { kind: Kind::Webhook, span, value });
    }
    for c in AUTHORIZATION.captures_iter(text) {
        let m = c.get(0).unwrap();
        out.push(Found { kind: Kind::Authorization, span: (m.start(), m.end()), value: group_span(&c, "value") });
    }
    named(text, secret_name, masked_value, &mut out);
    out
}

/// Context-aware review findings; narrower than `find`.
pub fn find_exposures(text: &str) -> Vec<Found> {
    let mut out = Vec::new();
    for span in TOKEN_SHAPES.iter(text) {
        let value = &text[span.0..span.1];
        if !DUMMY_CREDENTIAL.is_match(value) && !PEM_PLACEHOLDER.is_match(value) && PUBLIC_SSH_ALGORITHM.match_at(text, span.0).is_none() {
            out.push(Found { kind: Kind::Token, span, value: span });
        }
    }
    for (span, value) in webhooks::find(text, true) {
        out.push(Found { kind: Kind::Webhook, span, value });
    }
    named(text, exposure_name, exposure_value, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detected() {
        for text in [
            "LLM|123456789012|syntheticCredential98765",
            "OPENAI_API_KEY=sk-proj-abcdEFGH1234ijklMNOP",
            r#"os.environ["OPENAI_API_KEY"]="opaqueCredential98765""#,
            r#"api_key="opaqueCredential98765""#,
            "--api-key opaqueCredential98765",
            "x-api-key: opaqueCredential98765",
            "DB_PASSWORD=Opaque98765+/abc==",
            "-H 'Authorization: Bearer opaqueCredential98765' https://kubernetes.default.svc/",
        ] {
            assert!(!find_exposures(text).is_empty(), "{text}");
        }
    }

    #[test]
    fn not_secrets() {
        for text in [
            "TIKTOKEN_CACHE_DIR=/root/cache MAX_TOKENS=5000 TOKENIZERS_PARALLELISM=false",
            r#"api_key="your-key""#,
            "predicted = max(scores, key=scores.get)",
            r#"{"apiKeySource": "ANTHROPIC_API_KEY"}"#,
            "openssl pkcs12 -password=KEY_PASSWORD -in cert.pem",
            "(define (rk-free-variables-list expr) ...)",
            "token_owner=list_owner_address, secret_oid=3f2a9c1b7d4e5f60",
        ] {
            assert!(find_exposures(text).is_empty(), "{text}: {:?}", find_exposures(text));
        }
    }
}
