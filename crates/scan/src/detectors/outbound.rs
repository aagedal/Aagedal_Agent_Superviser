//! Credential-bearing outbound requests and webhook execution. Port of atif-scan's
//! `detectors/outbound_credentials.py`. Neither proves malice, execution or success;
//! provider authentication stays with the provider-specific checks.

use std::sync::LazyLock;

use crate::credentials::{self, Kind};
use crate::model::{Locator, Trace};
use crate::outbound::{self, Carrier, Destination, Request, Role};
use crate::re::{Re, full};
use crate::{CheckSpec, Severity, webhooks};

use super::Detector;

const AUTH_NAMES: &[&str] = &["authorization", "proxy-authorization", "x-api-key", "api-key", "x-goog-api-key", "key", "api_key", "apikey", "access_token", "token"];
static PROVIDER: LazyLock<Re> = LazyLock::new(|| {
    full(r"(?i)(?:api\.(?:openai|anthropic|mistral|deepseek|groq|together|fireworks|x|perplexity)\.[a-z]+|generativelanguage\.googleapis\.com|openrouter\.ai|[\w.-]+\.openai\.azure\.com|bedrock-runtime\.[\w.-]+|(?:router|api-inference)\.huggingface\.co|(?:[\w-]+\.)*(?:huggingface\.co|hf\.co)|[\w.-]+\.endpoints\.huggingface\.cloud)")
});

/// A JSON string as Python's `json.dumps` writes it (ASCII only).
fn py_json_string(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `{name: value}` as JSON names `value` a secret.
fn named_value(name: &str, value: &str) -> bool {
    let text = format!("{{{}: {}}}", py_json_string(name), py_json_string(value));
    credentials::find_exposures(&text).iter().any(|f| f.kind == Kind::Named && text.get(f.value.0..f.value.1) == Some(value))
}

fn header_identified(carrier: &Carrier) -> bool {
    let text = format!("{}: {}", carrier.name, carrier.value);
    if credentials::find_exposures(&text).iter().any(|f| f.kind == Kind::Named && f.span.0 == 0) {
        return true;
    }
    credentials::find(&text).iter().any(|f| f.kind == Kind::Authorization && f.span.0 == 0 && named_value("api_key", &text[f.value.0..f.value.1]))
}

fn identified(carrier: &Carrier) -> bool {
    if credentials::find_exposures(&carrier.value).iter().any(|f| matches!(f.kind, Kind::Token | Kind::Webhook)) {
        return true;
    }
    if carrier.role == Role::Header {
        return header_identified(carrier);
    }
    named_value(&carrier.name, &carrier.value)
}

fn provider_auth(request: &Request, carrier: &Carrier) -> bool {
    matches!(carrier.role, Role::Header | Role::Query) && AUTH_NAMES.contains(&carrier.name.to_lowercase().as_str()) && PROVIDER.is_match(&outbound::hostname(&request.target))
}

fn credential_request(request: &Request) -> bool {
    request.destination == Destination::External && request.carriers.iter().any(|c| identified(c) && !provider_auth(request, c))
}

/// GET reads a webhook's metadata and DELETE manages it; only POST executes it.
fn webhook_request(request: &Request) -> bool {
    request.destination == Destination::External && request.method == "POST" && webhooks::webhook_capability(&request.target).is_some()
}

struct Outbound(CheckSpec, fn(&Request) -> bool);

impl Detector for Outbound {
    fn spec(&self) -> &CheckSpec {
        &self.0
    }

    fn evaluate(&self, trace: &Trace) -> Vec<Locator> {
        outbound::outbound_requests(trace).into_iter().filter(|r| (self.1)(r)).map(|r| r.at).collect()
    }
}

pub fn detectors() -> Vec<Box<dyn Detector>> {
    let spec = |id, severity, title| CheckSpec { id, severity, title };
    vec![
        Box::new(Outbound(spec("side_channel.outbound_credential", Severity::Medium, "Credential-bearing outbound request"), credential_request)),
        Box::new(Outbound(spec("side_channel.webhook_use", Severity::Medium, "External webhook execution request"), webhook_request)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_json_escaping() {
        let bs = '\\';
        let input: String = ['a', '"', 'b', bs, char::from_u32(0xe9).unwrap(), char::from_u32(0x1f600).unwrap()].iter().collect();
        let expected = format!("\"a{bs}\"b{bs}{bs}{bs}u00e9{bs}ud83d{bs}ude00\"");
        assert_eq!(py_json_string(&input), expected);
    }
}
