//! Classifies a subscription body and extracts proxy endpoints for stub detection.
//!
//! We never convert share links ourselves: mihomo's file proxy-provider already
//! understands raw and base64 link lists (verified on v1.19.31). Link parsing
//! here is only deep enough to read `name`, `server` and `port`.

use serde_norway::{Mapping, Value};

use super::headers::{decode_base64, percent_decode};

/// Share-link schemes mihomo's converter understands. `http(s)` is deliberately
/// absent: provider messages often contain plain web links. `wireguard`/`wg` and
/// `mieru` links pass `mihomo -t` but fail at runtime, so they are not treated as
/// proxies either.
const LINK_SCHEMES: &[&str] = &[
    "vless",
    "vmess",
    "trojan",
    "ss",
    "ssr",
    "hysteria",
    "hysteria2",
    "hy2",
    "tuic",
    "socks",
    "socks5",
    "anytls",
];

/// libyaml handles deeply nested flow collections in quadratic time (80 KB of `[`
/// takes seconds), so absurd nesting is refused before parsing. Real configs stay
/// below ten levels.
const MAX_FLOW_DEPTH: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// A full or partial mihomo/clash YAML config.
    Mihomo,
    /// A list of share links (plain or base64).
    Links,
    /// An Xray JSON config (Happ), converted to mihomo proxies.
    XrayJson,
}

impl Format {
    pub fn as_str(self) -> &'static str {
        match self {
            Format::Mihomo => "mihomo",
            Format::Links => "links",
            Format::XrayJson => "xray-json",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub name: String,
    pub server: String,
    pub port: u16,
}

#[derive(Debug, Clone)]
pub struct Content {
    pub format: Format,
    /// Parsed YAML (merge keys applied) for [`Format::Mihomo`], or the converted
    /// `proxies:` document for [`Format::XrayJson`].
    pub yaml: Option<Value>,
    /// Decoded, newline-separated links for [`Format::Links`].
    pub links: Option<String>,
    pub endpoints: Vec<Endpoint>,
    /// The YAML references proxy-providers (proxies we cannot inspect).
    pub has_providers: bool,
    /// Conversion caveats worth logging (Xray features mihomo lacks).
    pub notes: Vec<String>,
}

pub fn parse(body: &[u8], content_type: Option<&str>) -> Result<Content, String> {
    let text = String::from_utf8_lossy(body);
    let text = text.trim_start_matches('\u{feff}').trim();
    if text.is_empty() {
        return Err("empty body".into());
    }
    let lower_head: String = text
        .chars()
        .take(64)
        .collect::<String>()
        .to_ascii_lowercase();
    if content_type.is_some_and(|c| c.to_ascii_lowercase().contains("text/html"))
        || lower_head.starts_with("<!doctype")
        || lower_head.starts_with("<html")
    {
        return Err("got an HTML page: the panel did not recognise the client".into());
    }
    if text.starts_with('{') || text.starts_with('[') {
        return parse_json(text);
    }
    if let Some(content) = parse_yaml(text)? {
        return Ok(content);
    }
    if let Some(content) = parse_links(text) {
        return Ok(content);
    }
    if let Some(content) = decode_base64(text)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .and_then(|decoded| parse_links(&decoded))
    {
        return Ok(content);
    }
    if text.starts_with("happ://") {
        return Err("got an encrypted happ:// link, which only Happ can decrypt".into());
    }
    Err("unrecognised subscription format".into())
}

fn parse_json(text: &str) -> Result<Content, String> {
    let json: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("invalid JSON body: {e}"))?;
    if !super::xray::is_xray(&json) {
        let kind = if text.contains("\"outbounds\"") {
            "a sing-box JSON config"
        } else {
            "JSON"
        };
        return Err(format!(
            "got {kind}; supported are mihomo YAML, share links and Xray JSON (try client flclashx)"
        ));
    }
    let converted = super::xray::convert(&json)?;
    let endpoints = converted
        .proxies
        .iter()
        .filter_map(Value::as_mapping)
        .filter_map(yaml_endpoint)
        .collect();
    let mut root = Mapping::new();
    root.insert("proxies".into(), Value::Sequence(converted.proxies));
    Ok(Content {
        format: Format::XrayJson,
        yaml: Some(Value::Mapping(root)),
        links: None,
        endpoints,
        has_providers: false,
        notes: converted.warnings,
    })
}

/// `Ok(None)` when the text is not a clash config at all; `Err` when it clearly is
/// one (a top-level `proxies:` & co.) but does not parse, so the reason is logged.
fn parse_yaml(text: &str) -> Result<Option<Content>, String> {
    const KEYS: [&str; 3] = ["proxies", "proxy-providers", "proxy-groups"];
    let looks_like_config = text.lines().any(|line| {
        KEYS.iter().any(|key| {
            line.strip_prefix(key)
                .is_some_and(|rest| rest.starts_with(':'))
        })
    });
    if flow_depth_exceeds(text, MAX_FLOW_DEPTH) {
        return if looks_like_config {
            Err(format!("YAML nesting deeper than {MAX_FLOW_DEPTH} levels"))
        } else {
            Ok(None)
        };
    }
    let mut value: Value = match serde_norway::from_str(text) {
        Ok(value) => value,
        Err(e) if looks_like_config => {
            return Err(crate::util::sanitize(&format!("invalid YAML config: {e}")));
        }
        Err(_) => return Ok(None),
    };
    let Some(map) = value.as_mapping() else {
        return Ok(None);
    };
    if !KEYS.iter().any(|key| map.contains_key(*key)) {
        return Ok(None);
    }
    value
        .apply_merge()
        .map_err(|e| format!("invalid YAML merge keys: {e}"))?;
    let Some(map) = value.as_mapping() else {
        return Ok(None);
    };
    let endpoints = map
        .get("proxies")
        .and_then(Value::as_sequence)
        .map(|seq| {
            seq.iter()
                .filter_map(Value::as_mapping)
                .filter_map(yaml_endpoint)
                .collect()
        })
        .unwrap_or_default();
    let has_providers = map
        .get("proxy-providers")
        .and_then(Value::as_mapping)
        .is_some_and(|m| !m.is_empty());
    Ok(Some(Content {
        format: Format::Mihomo,
        yaml: Some(value),
        links: None,
        endpoints,
        has_providers,
        notes: Vec::new(),
    }))
}

/// Deepest nesting of flow collections (`[`/`{`), skipping quoted scalars and
/// comments. Approximate on purpose: it only has to be right for inputs that
/// would be slow to parse.
fn flow_depth_exceeds(text: &str, max: usize) -> bool {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut quote: Option<u8> = None;
    let mut comment = false;
    // Last non-blank byte, to tell `'quoted'` from an apostrophe in a plain scalar.
    let mut prev = b'\n';
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        i += 1;
        if comment {
            if b == b'\n' {
                comment = false;
                prev = b;
            }
            continue;
        }
        if let Some(q) = quote {
            if q == b'"' && b == b'\\' {
                i += 1;
            } else if b == q {
                quote = None;
                prev = b;
            }
            continue;
        }
        match b {
            b'#' if i < 2 || bytes[i - 2].is_ascii_whitespace() => comment = true,
            b'\'' | b'"' if matches!(prev, b'[' | b'{' | b',' | b':' | b'-' | b'\n') => {
                quote = Some(b)
            }
            b'[' | b'{' => {
                depth += 1;
                if depth > max {
                    return true;
                }
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
        if b == b'\n' || !b.is_ascii_whitespace() {
            prev = b;
        }
    }
    false
}

fn yaml_endpoint(proxy: &Mapping) -> Option<Endpoint> {
    let str_of = |key: &str| proxy.get(key).and_then(Value::as_str).map(str::to_owned);
    let port = match proxy.get("port")? {
        Value::Number(n) => n.as_u64().and_then(|p| u16::try_from(p).ok())?,
        Value::String(s) => s.split(['-', ',']).next()?.trim().parse().ok()?,
        _ => return None,
    };
    Some(Endpoint {
        name: str_of("name").unwrap_or_default(),
        server: str_of("server")?,
        port,
    })
}

fn parse_links(text: &str) -> Option<Content> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| {
            l.split_once("://").is_some_and(|(scheme, _)| {
                LINK_SCHEMES.contains(&scheme.to_ascii_lowercase().as_str())
            })
        })
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(Content {
        format: Format::Links,
        yaml: None,
        endpoints: lines.iter().filter_map(|l| link_endpoint(l)).collect(),
        links: Some(lines.join("\n") + "\n"),
        has_providers: false,
        notes: Vec::new(),
    })
}

fn link_endpoint(link: &str) -> Option<Endpoint> {
    let (scheme, rest) = link.split_once("://")?;
    let (rest, fragment) = rest.split_once('#').unwrap_or((rest, ""));
    let name = percent_decode(fragment);
    match scheme.to_ascii_lowercase().as_str() {
        "vmess" if !rest.contains('@') => {
            let json: serde_json::Value = serde_json::from_slice(&decode_base64(rest)?).ok()?;
            let port = match &json["port"] {
                serde_json::Value::Number(n) => n.as_u64()?,
                serde_json::Value::String(s) => s.parse().ok()?,
                _ => return None,
            };
            Some(Endpoint {
                name: json["ps"].as_str().unwrap_or(&name).to_owned(),
                server: json["add"].as_str()?.to_owned(),
                port: u16::try_from(port).ok()?,
            })
        }
        "ssr" => {
            // ssr://base64(host:port:protocol:method:obfs:password_b64/?params)
            let decoded = String::from_utf8(decode_base64(rest)?).ok()?;
            let mut parts = decoded.rsplitn(6, ':').collect::<Vec<_>>();
            parts.reverse();
            Some(Endpoint {
                name,
                server: parts.first()?.to_string(),
                port: parts.get(1)?.parse().ok()?,
            })
        }
        "ss" if !rest.contains('@') => {
            // SIP002 legacy form: ss://base64(method:password@host:port)
            let decoded = String::from_utf8(decode_base64(rest.split('?').next()?)?).ok()?;
            authority_endpoint(decoded.rsplit_once('@')?.1, name, None)
        }
        scheme => {
            // userinfo may contain `/` (base64, passwords), so find `@` first.
            let before_query = rest.split('?').next()?;
            let host_part = before_query
                .rsplit_once('@')
                .map_or(before_query, |(_, host)| host);
            let authority = host_part.split('/').next()?;
            let default_port = matches!(scheme, "hysteria2" | "hy2" | "tuic").then_some(443);
            authority_endpoint(authority, name, default_port)
        }
    }
}

/// `host:port`, `[v6]:port`, or a bare host when the scheme has a default port.
/// Port hopping lists (`443,20000-30000`) yield their first port.
fn authority_endpoint(
    authority: &str,
    name: String,
    default_port: Option<u16>,
) -> Option<Endpoint> {
    let (server, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        (host, tail.strip_prefix(':'))
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    let port = match port {
        Some(port) => port.split([',', '-']).next()?.trim().parse().ok()?,
        None => default_port?,
    };
    Some(Endpoint {
        name,
        server: server.to_owned(),
        port,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    const REMNAWAVE_STUB_YAML: &str = r#"
mixed-port: 7890
mode: global
proxies:
  - name: App not supported
    type: vless
    server: 0.0.0.0
    port: 1
    uuid: 00000000-0000-0000-0000-000000000000
proxy-groups:
  - name: '→ Remnawave'
    type: select
    proxies: [App not supported]
rules:
  - MATCH,→ Remnawave
"#;

    #[test]
    fn parses_mihomo_yaml() {
        let content = parse(REMNAWAVE_STUB_YAML.as_bytes(), Some("text/yaml")).unwrap();
        assert_eq!(content.format, Format::Mihomo);
        assert_eq!(
            content.endpoints,
            vec![Endpoint {
                name: "App not supported".into(),
                server: "0.0.0.0".into(),
                port: 1
            }]
        );
        assert!(!content.has_providers);
    }

    #[test]
    fn applies_yaml_merge_keys() {
        let yaml =
            "base: &b {type: select, proxies: [DIRECT]}\nproxy-groups:\n  - <<: *b\n    name: G\n";
        let content = parse(yaml.as_bytes(), None).unwrap();
        let group = &content.yaml.unwrap()["proxy-groups"][0];
        assert_eq!(group["type"].as_str(), Some("select"));
        assert_eq!(group["name"].as_str(), Some("G"));
    }

    #[test]
    fn parses_links_plain_and_base64() {
        let links = "vless://11111111-2222-3333-4444-555555555555@nl.example.com:443?security=reality#%F0%9F%87%B3%F0%9F%87%B1%20NL\n\
                     trojan://pass@[2001:db8::1]:8443?sni=x#TR\n\
                     ss://YWVzLTEyOC1nY206cGFzcw@ss.example.com:8388#SS\n";
        for body in [
            links.to_owned(),
            base64::engine::general_purpose::STANDARD.encode(links),
        ] {
            let content = parse(body.as_bytes(), Some("text/plain")).unwrap();
            assert_eq!(content.format, Format::Links);
            let names: Vec<_> = content.endpoints.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, ["🇳🇱 NL", "TR", "SS"]);
            assert_eq!(content.endpoints[1].server, "2001:db8::1");
            assert_eq!(content.endpoints[1].port, 8443);
            assert_eq!(content.links.as_deref().unwrap().lines().count(), 3);
        }
    }

    #[test]
    fn parses_vmess_ssr_and_legacy_ss() {
        let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
        let vmess = format!(
            "vmess://{}",
            b64(r#"{"v":"2","ps":"VM","add":"vm.example","port":"443","id":"x"}"#)
        );
        let ssr = format!(
            "ssr://{}",
            b64("ssr.example:9000:origin:aes-256-cfb:plain:cGFzcw/?remarks=x")
        );
        let ss = format!("ss://{}#Legacy", b64("aes-256-gcm:pw@1.2.3.4:8388"));
        let content = parse(format!("{vmess}\n{ssr}\n{ss}").as_bytes(), None).unwrap();
        let got: Vec<_> = content
            .endpoints
            .iter()
            .map(|e| (e.name.as_str(), e.server.as_str(), e.port))
            .collect();
        assert_eq!(
            got,
            [
                ("VM", "vm.example", 443),
                ("", "ssr.example", 9000),
                ("Legacy", "1.2.3.4", 8388)
            ]
        );
    }

    #[test]
    fn rejects_non_configs() {
        let err = |body: &str, ct: Option<&str>| parse(body.as_bytes(), ct).unwrap_err();
        assert!(err("", None).contains("empty"));
        assert!(err("<!DOCTYPE html><html></html>", None).contains("HTML"));
        assert!(err("anything", Some("text/html; charset=utf-8")).contains("HTML"));
        assert!(err(r#"{"outbounds":[{"protocol":"freedom"}]}"#, None).contains("without proxy"));
        assert!(err(r#"{"outbounds":[{"type":"vless"}]}"#, None).contains("sing-box"));
        assert!(err("happ://crypt5/abc", None).contains("happ"));
        assert!(err("just some text", None).contains("unrecognised"));
        assert!(err("key: value", None).contains("unrecognised"));
        assert!(err("proxies:\n  - {name: a\n", None).contains("invalid YAML"));
        assert!(err("wireguard://k@1.2.3.4:51820#WG", None).contains("unrecognised"));
    }

    #[test]
    fn refuses_pathological_nesting_quickly() {
        let deep = format!("proxies: {}{}", "[".repeat(100_000), "]".repeat(100_000));
        let started = std::time::Instant::now();
        assert!(
            parse(deep.as_bytes(), None)
                .unwrap_err()
                .contains("nesting")
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        // Brackets inside quotes and comments do not count.
        let quoted = format!(
            "proxies:\n  - {{name: '{}', type: ss, server: a.example, port: 1}} # {}\n",
            "[".repeat(500),
            "{".repeat(500)
        );
        assert!(!flow_depth_exceeds(&quoted, MAX_FLOW_DEPTH));
        assert!(!flow_depth_exceeds("name: it's [x] {y}\n", 1));
    }

    #[test]
    fn link_endpoint_edge_cases() {
        let ep = |link: &str| link_endpoint(link).map(|e| (e.server, e.port));
        let own = |s: &str, p: u16| Some((s.to_owned(), p));
        assert_eq!(ep("hy2://auth@hy.example?sni=x#H"), own("hy.example", 443));
        assert_eq!(
            ep("hysteria2://a@h.example:443,20000-30000/?x#H"),
            own("h.example", 443)
        );
        assert_eq!(
            ep("hy2://a@h.example:20000-30000#H"),
            own("h.example", 20000)
        );
        assert_eq!(ep("tuic://u:p@[2001:db8::2]#T"), own("2001:db8::2", 443));
        assert_eq!(
            ep("trojan://pa/ss@t.example:8443/?sni=x#T"),
            own("t.example", 8443)
        );
        assert_eq!(ep("vless://id@v.example#no-port"), None);
    }
}
