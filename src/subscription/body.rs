//! Classifies a subscription body and extracts proxy endpoints for stub detection.
//!
//! We never convert share links ourselves: mihomo's file proxy-provider already
//! understands raw and base64 link lists (verified on v1.19.31). Link parsing
//! here is only deep enough to read `name`, `server` and `port`.

use serde_norway::{Mapping, Value};

use super::headers::{decode_base64, percent_decode};

/// Share-link schemes mihomo's converter understands. `http(s)` is deliberately
/// absent: provider messages often contain plain web links.
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
    "wireguard",
    "wg",
    "mieru",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// A full or partial mihomo/clash YAML config.
    Mihomo,
    /// A list of share links (plain or base64).
    Links,
}

impl Format {
    pub fn as_str(self) -> &'static str {
        match self {
            Format::Mihomo => "mihomo",
            Format::Links => "links",
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
    /// Parsed YAML (merge keys applied) for [`Format::Mihomo`].
    pub yaml: Option<Value>,
    /// Decoded, newline-separated links for [`Format::Links`].
    pub links: Option<String>,
    pub endpoints: Vec<Endpoint>,
    /// The YAML references proxy-providers (proxies we cannot inspect).
    pub has_providers: bool,
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
        return Err(describe_json(text));
    }
    if let Some(content) = parse_yaml(text) {
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

fn describe_json(text: &str) -> String {
    let kind = if text.contains("\"outbounds\"") && text.contains("\"protocol\"") {
        "an Xray JSON config"
    } else if text.contains("\"outbounds\"") {
        "a sing-box JSON config"
    } else {
        "JSON"
    };
    format!("got {kind}; only mihomo YAML and share links are supported (try client flclashx)")
}

fn parse_yaml(text: &str) -> Option<Content> {
    let mut value: Value = serde_norway::from_str(text).ok()?;
    let map = value.as_mapping()?;
    let has = |key: &str| map.contains_key(key);
    if !has("proxies") && !has("proxy-providers") && !has("proxy-groups") {
        return None;
    }
    value.apply_merge().ok()?;
    let map = value.as_mapping()?;
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
    Some(Content {
        format: Format::Mihomo,
        yaml: Some(value),
        links: None,
        endpoints,
        has_providers,
    })
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
            authority_endpoint(decoded.rsplit_once('@')?.1, name)
        }
        _ => {
            let authority = rest.split(['/', '?']).next()?;
            let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
            authority_endpoint(authority, name)
        }
    }
}

fn authority_endpoint(authority: &str, name: String) -> Option<Endpoint> {
    let (server, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (host, port) = rest.split_once("]:")?;
        (host, port)
    } else {
        authority.rsplit_once(':')?
    };
    Some(Endpoint {
        name,
        server: server.to_owned(),
        port: port.parse().ok()?,
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
        assert!(err(r#"{"outbounds":[{"protocol":"vless"}]}"#, None).contains("Xray"));
        assert!(err(r#"{"outbounds":[{"type":"vless"}]}"#, None).contains("sing-box"));
        assert!(err("happ://crypt5/abc", None).contains("happ"));
        assert!(err("just some text", None).contains("unrecognised"));
        assert!(err("key: value", None).contains("unrecognised"));
    }
}
