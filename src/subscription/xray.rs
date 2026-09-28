//! Xray JSON subscription → mihomo proxies.
//!
//! Panels answer Happ with Xray JSON when `serveJsonAtBaseSubscription` (Remnawave)
//! or `USE_CUSTOM_JSON_FOR_HAPP` (Marzban) is enabled. The field mapping mirrors
//! Remnawave's own mihomo generator (`mihomo.generator.service.ts`), i.e. the
//! mapping the panel itself uses for the same hosts, so results match what a
//! FlClashX user of the same panel would get.
//!
//! Accepted shapes:
//! * Remnawave: an array of full Xray configs, one per host, each with `remarks`
//!   and the host's outbound tagged `proxy`;
//! * a single Xray config: every proxy outbound becomes a proxy named by its tag.

use serde_json::Value as Json;
use serde_norway::{Mapping, Value};

/// uTLS fingerprints mihomo understands (Remnawave's `FINGERPRINTS`).
const FINGERPRINTS: &[&str] = &[
    "chrome", "firefox", "safari", "ios", "android", "edge", "360", "qq", "random",
];

/// Outbound protocols that are routing helpers, not proxies.
const NON_PROXY: &[&str] = &["freedom", "blackhole", "dns", "loopback"];

/// xhttp `extra` fields → mihomo `xhttp-opts` (third value: stringify).
const XHTTP_FIELDS: &[(&str, &str, bool)] = &[
    ("noGRPCHeader", "no-grpc-header", false),
    ("xPaddingBytes", "x-padding-bytes", true),
    ("xPaddingObfsMode", "x-padding-obfs-mode", false),
    ("xPaddingKey", "x-padding-key", false),
    ("xPaddingHeader", "x-padding-header", false),
    ("xPaddingPlacement", "x-padding-placement", false),
    ("xPaddingMethod", "x-padding-method", false),
    ("uplinkHTTPMethod", "uplink-http-method", false),
    ("sessionIDPlacement", "session-placement", false),
    ("sessionIDKey", "session-key", false),
    ("sessionIDTable", "session-table", false),
    ("sessionIDLength", "session-length", true),
    ("seqPlacement", "seq-placement", false),
    ("seqKey", "seq-key", false),
    ("uplinkDataPlacement", "uplink-data-placement", false),
    ("uplinkDataKey", "uplink-data-key", false),
    ("uplinkChunkSize", "uplink-chunk-size", false),
    ("scMaxEachPostBytes", "sc-max-each-post-bytes", false),
    ("scMinPostsIntervalMs", "sc-min-posts-interval-ms", false),
];

const XMUX_FIELDS: &[(&str, &str, bool)] = &[
    ("maxConnections", "max-connections", true),
    ("maxConcurrency", "max-concurrency", true),
    ("cMaxReuseTimes", "c-max-reuse-times", true),
    ("hMaxRequestTimes", "h-max-request-times", true),
    ("hMaxReusableSecs", "h-max-reusable-secs", true),
    ("hKeepAlivePeriod", "h-keep-alive-period", false),
];

pub struct Converted {
    pub proxies: Vec<Value>,
    /// Features that could not be carried over (logged, never fatal).
    pub warnings: Vec<String>,
}

/// Whether a JSON document looks like an Xray config (or a Remnawave list of them).
pub fn is_xray(json: &Json) -> bool {
    let config = match json {
        Json::Array(items) => items.first(),
        other => Some(other),
    };
    config
        .and_then(|c| c["outbounds"].as_array())
        .is_some_and(|obs| obs.iter().any(|o| o.get("protocol").is_some()))
}

pub fn convert(json: &Json) -> Result<Converted, String> {
    let mut out = Converted {
        proxies: Vec::new(),
        warnings: Vec::new(),
    };
    // Proxy names must not shadow mihomo's built-in policies.
    let mut names: std::collections::HashSet<String> = crate::mihomo::profile::BUILTIN_NAMES
        .iter()
        .map(|n| n.to_string())
        .collect();
    let mut push = |out: &mut Converted, outbound: &Json, name: &str| {
        let name = unique_name(&mut names, name);
        match convert_outbound(outbound, &name, &mut out.warnings) {
            Ok(Some(proxy)) => out.proxies.push(Value::Mapping(proxy)),
            Ok(None) => {}
            Err(e) => out.warnings.push(format!("{name}: skipped, {e}")),
        }
    };
    match json {
        Json::Array(configs) => {
            for config in configs {
                let outbounds = config["outbounds"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                let chosen = outbounds
                    .iter()
                    .find(|o| o["tag"] == "proxy")
                    .or_else(|| outbounds.iter().find(|o| is_proxy(o)));
                let Some(outbound) = chosen else { continue };
                let name = config["remarks"]
                    .as_str()
                    .or_else(|| outbound["tag"].as_str())
                    .unwrap_or("proxy");
                push(&mut out, outbound, name);
            }
        }
        Json::Object(_) => {
            let outbounds: Vec<&Json> = json["outbounds"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|o| is_proxy(o))
                .collect();
            let single_remark = json["remarks"].as_str().filter(|_| outbounds.len() == 1);
            for outbound in outbounds {
                let name = single_remark
                    .or_else(|| outbound["tag"].as_str())
                    .unwrap_or("proxy");
                push(&mut out, outbound, name);
            }
        }
        _ => return Err("not an Xray config".into()),
    }
    if out.proxies.is_empty() && out.warnings.is_empty() {
        return Err("Xray JSON without proxy outbounds".into());
    }
    Ok(out)
}

fn is_proxy(outbound: &Json) -> bool {
    outbound["protocol"]
        .as_str()
        .is_some_and(|p| !NON_PROXY.contains(&p))
}

fn unique_name(seen: &mut std::collections::HashSet<String>, base: &str) -> String {
    let mut name = base.to_owned();
    let mut n = 2;
    while !seen.insert(name.clone()) {
        name = format!("{base} {n}");
        n += 1;
    }
    name
}

fn set(map: &mut Mapping, key: &str, value: impl Into<Value>) {
    map.insert(Value::from(key), value.into());
}

fn json_to_yaml(json: &Json) -> Value {
    serde_norway::to_value(json).unwrap_or(Value::Null)
}

fn json_str(json: &Json) -> Option<&str> {
    json.as_str().filter(|s| !s.is_empty())
}

fn port_of(json: &Json) -> Option<u16> {
    match json {
        Json::Number(n) => n.as_u64().and_then(|p| u16::try_from(p).ok()),
        Json::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// `vnext[0]`/`servers[0]` (classic) or the flat single-server form of Xray ≥ 25.
fn server_entry(settings: &Json) -> &Json {
    for key in ["vnext", "servers"] {
        if let Some(first) = settings[key].as_array().and_then(|a| a.first()) {
            return first;
        }
    }
    settings
}

fn convert_outbound(
    ob: &Json,
    name: &str,
    warnings: &mut Vec<String>,
) -> Result<Option<Mapping>, String> {
    let protocol = ob["protocol"].as_str().unwrap_or_default();
    let settings = &ob["settings"];
    let server = server_entry(settings);
    let user = server["users"]
        .as_array()
        .and_then(|u| u.first())
        .unwrap_or(server);
    let address = json_str(&server["address"]).ok_or("no server address")?;
    let port = port_of(&server["port"]).ok_or("no server port")?;
    let stream = &ob["streamSettings"];

    let mut node = Mapping::new();
    set(&mut node, "name", name);
    let kind = match protocol {
        "vless" => "vless",
        "vmess" => "vmess",
        "trojan" => "trojan",
        "shadowsocks" => "ss",
        "hysteria" => "hysteria2",
        "socks" => "socks5",
        "http" => "http",
        other => return Err(format!("unsupported protocol {other:?}")),
    };
    set(&mut node, "type", kind);
    set(&mut node, "server", address);
    set(&mut node, "port", port);
    set(&mut node, "udp", true);

    match protocol {
        "vless" => vless(&mut node, user)?,
        "vmess" => vmess(&mut node, user)?,
        "trojan" => {
            set(
                &mut node,
                "password",
                json_str(&server["password"]).ok_or("no trojan password")?,
            );
        }
        "shadowsocks" => shadowsocks(&mut node, server, stream)?,
        "hysteria" => {
            hysteria(&mut node, stream, name, warnings)?;
            return Ok(Some(node));
        }
        "socks" | "http" => {
            if let (Some(u), Some(p)) = (json_str(&user["user"]), json_str(&user["pass"])) {
                set(&mut node, "username", u);
                set(&mut node, "password", p);
            }
        }
        _ => unreachable!(),
    }

    apply_security(&mut node, kind, stream, name, warnings)?;
    apply_transport(&mut node, stream)?;

    if stream["sockopt"]["dialerProxy"].is_string() {
        warnings.push(format!(
            "{name}: sockopt.dialerProxy (chained/fragment outbound) not converted"
        ));
    }
    if ob["mux"]["enabled"].as_bool() == Some(true) {
        warnings.push(format!("{name}: Xray mux not converted (mihomo uses smux)"));
    }
    Ok(Some(node))
}

fn vless(node: &mut Mapping, user: &Json) -> Result<(), String> {
    set(node, "uuid", json_str(&user["id"]).ok_or("no vless id")?);
    set(node, "packet-encoding", "xudp");
    if user["flow"] == "xtls-rprx-vision" {
        set(node, "flow", "xtls-rprx-vision");
    }
    if let Some(enc) = json_str(&user["encryption"]).filter(|e| *e != "none") {
        set(node, "encryption", enc);
    }
    Ok(())
}

fn vmess(node: &mut Mapping, user: &Json) -> Result<(), String> {
    set(node, "uuid", json_str(&user["id"]).ok_or("no vmess id")?);
    set(node, "alterId", user["alterId"].as_u64().unwrap_or(0));
    set(
        node,
        "cipher",
        json_str(&user["security"]).unwrap_or("auto"),
    );
    Ok(())
}

fn shadowsocks(node: &mut Mapping, server: &Json, stream: &Json) -> Result<(), String> {
    let network = stream["network"].as_str().unwrap_or("tcp");
    if !matches!(network, "tcp" | "raw") {
        return Err(format!(
            "shadowsocks over {network} is not supported by mihomo"
        ));
    }
    set(
        node,
        "cipher",
        json_str(&server["method"]).ok_or("no ss method")?,
    );
    set(
        node,
        "password",
        json_str(&server["password"]).ok_or("no ss password")?,
    );
    if server["uot"].as_bool() == Some(true) {
        set(node, "udp-over-tcp", true);
        if let Some(v) = server["UoTVersion"].as_u64() {
            set(node, "udp-over-tcp-version", v);
        }
    }
    Ok(())
}

fn hysteria(
    node: &mut Mapping,
    stream: &Json,
    name: &str,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    let auth = json_str(&stream["hysteriaSettings"]["auth"]).ok_or("no hysteria auth")?;
    set(node, "password", auth);
    apply_hysteria_tls(node, stream);
    // `quicParams` only tunes QUIC (congestion control, windows): mihomo's own
    // defaults work with the same server. Masks (obfs) would not.
    if let Some(mask) = stream["finalmask"].as_object()
        && mask.keys().any(|key| key != "quicParams")
    {
        warnings.push(format!(
            "{name}: hysteria finalmask (obfs/port hopping) not converted"
        ));
    }
    Ok(())
}

fn fingerprint(raw: Option<&str>) -> &'static str {
    let raw = raw.unwrap_or_default().to_ascii_lowercase();
    FINGERPRINTS
        .iter()
        .find(|fp| !raw.is_empty() && raw.contains(*fp))
        .copied()
        .unwrap_or("chrome")
}

fn apply_security(
    node: &mut Mapping,
    kind: &str,
    stream: &Json,
    name: &str,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    let security = stream["security"].as_str().unwrap_or("none");
    // mihomo: vless/vmess take `servername`, trojan and http take `sni`.
    let sni_key = if matches!(kind, "trojan" | "http") {
        "sni"
    } else {
        "servername"
    };
    let opts = match security {
        "tls" => &stream["tlsSettings"],
        "reality" => &stream["realitySettings"],
        _ => {
            set(node, "client-fingerprint", "chrome");
            return Ok(());
        }
    };
    set(node, "tls", true);
    if let Some(sni) = opts["serverName"].as_str() {
        set(node, sni_key, sni);
    }
    set(
        node,
        "client-fingerprint",
        fingerprint(opts["fingerprint"].as_str()),
    );
    if security == "tls" {
        if let Some(alpn) = alpn_list(&opts["alpn"]) {
            set(node, "alpn", alpn);
        }
        if opts["allowInsecure"].as_bool() == Some(true) {
            set(node, "skip-cert-verify", true);
        }
        // Certificate pinning maps to mihomo's `fingerprint` (sha256 of the
        // certificate); it tightens verification instead of disabling it.
        if let Some(pins) = json_str(&opts["pinnedPeerCertSha256"]) {
            let mut valid = pins
                .split([',', '~', ' '])
                .map(|p| p.trim().replace(':', "").to_ascii_lowercase())
                .filter(|p| p.len() == 64 && p.bytes().all(|b| b.is_ascii_hexdigit()));
            match valid.next() {
                Some(pin) => {
                    set(node, "fingerprint", pin);
                    if valid.next().is_some() {
                        warnings.push(format!(
                            "{name}: several pinned certificates, only the first is used"
                        ));
                    }
                }
                None => warnings.push(format!(
                    "{name}: pinnedPeerCertSha256 is not a hex SHA-256, pin ignored"
                )),
            }
        }
    } else {
        let mut reality = Mapping::new();
        // Xray ≥ 25.x renamed publicKey to password on the client side.
        let pk = json_str(&opts["publicKey"])
            .or_else(|| json_str(&opts["password"]))
            .ok_or("reality without publicKey")?;
        set(&mut reality, "public-key", pk);
        set(
            &mut reality,
            "short-id",
            opts["shortId"].as_str().unwrap_or_default(),
        );
        set(node, "reality-opts", Value::Mapping(reality));
    }
    Ok(())
}

fn alpn_list(json: &Json) -> Option<Value> {
    let list: Vec<Value> = match json {
        Json::Array(items) => items
            .iter()
            .filter_map(|a| a.as_str())
            .map(Value::from)
            .collect(),
        Json::String(s) => s.split(',').map(|a| Value::from(a.trim())).collect(),
        _ => return None,
    };
    (!list.is_empty()).then_some(Value::Sequence(list))
}

fn apply_hysteria_tls(node: &mut Mapping, stream: &Json) {
    let tls = &stream["tlsSettings"];
    if let Some(sni) = tls["serverName"].as_str() {
        set(node, "sni", sni);
    }
    if let Some(alpn) = alpn_list(&tls["alpn"]) {
        set(node, "alpn", alpn);
    }
    if tls["allowInsecure"].as_bool() == Some(true) {
        set(node, "skip-cert-verify", true);
    }
}

fn apply_transport(node: &mut Mapping, stream: &Json) -> Result<(), String> {
    let network = stream["network"].as_str().unwrap_or("tcp");
    match network {
        "tcp" | "raw" => {
            let header = &stream["tcpSettings"]["header"];
            let header = if header.is_null() {
                &stream["rawSettings"]["header"]
            } else {
                header
            };
            if header["type"] == "http" {
                set(node, "network", "http");
                let mut opts = Mapping::new();
                let request = &header["request"];
                if let Some(paths) = request["path"].as_array() {
                    set(&mut opts, "path", json_to_yaml(&Json::Array(paths.clone())));
                }
                if request["headers"].is_object() {
                    set(&mut opts, "headers", json_to_yaml(&request["headers"]));
                }
                if !opts.is_empty() {
                    set(node, "http-opts", Value::Mapping(opts));
                }
            } else {
                set(node, "network", "tcp");
            }
        }
        "ws" | "httpupgrade" => {
            let ws = if network == "ws" {
                &stream["wsSettings"]
            } else {
                &stream["httpupgradeSettings"]
            };
            set(node, "network", "ws");
            let mut opts = Mapping::new();
            let (path, early) = split_early_data(ws["path"].as_str().unwrap_or_default());
            if let Some(early) = early {
                set(&mut opts, "max-early-data", early);
                set(
                    &mut opts,
                    "early-data-header-name",
                    "Sec-WebSocket-Protocol",
                );
            }
            if !path.is_empty() {
                set(&mut opts, "path", path);
            }
            let mut headers = Mapping::new();
            if let Some(host) = json_str(&ws["host"]) {
                set(&mut headers, "Host", host);
            }
            if let Some(extra) = ws["headers"].as_object() {
                for (k, v) in extra {
                    if let Some(v) = v.as_str() {
                        set(&mut headers, k, v);
                    }
                }
            }
            set(&mut opts, "headers", Value::Mapping(headers));
            if network == "httpupgrade" {
                set(&mut opts, "v2ray-http-upgrade", true);
                set(&mut opts, "v2ray-http-upgrade-fast-open", true);
            }
            set(node, "ws-opts", Value::Mapping(opts));
        }
        "grpc" => {
            set(node, "network", "grpc");
            let mut opts = Mapping::new();
            set(
                &mut opts,
                "grpc-service-name",
                stream["grpcSettings"]["serviceName"]
                    .as_str()
                    .unwrap_or_default(),
            );
            set(node, "grpc-opts", Value::Mapping(opts));
        }
        "xhttp" | "splithttp" => {
            let xhttp = if network == "xhttp" {
                &stream["xhttpSettings"]
            } else {
                &stream["splithttpSettings"]
            };
            set(node, "network", "xhttp");
            set(node, "xhttp-opts", Value::Mapping(xhttp_opts(xhttp)));
        }
        "h2" | "http" => {
            set(node, "network", "h2");
            let h2 = &stream["httpSettings"];
            let mut opts = Mapping::new();
            if let Some(path) = h2["path"].as_str() {
                set(&mut opts, "path", path);
            }
            if let Some(hosts) = h2["host"].as_array() {
                set(&mut opts, "host", json_to_yaml(&Json::Array(hosts.clone())));
            }
            set(node, "h2-opts", Value::Mapping(opts));
        }
        other => return Err(format!("transport {other:?} is not supported by mihomo")),
    }
    Ok(())
}

/// Xray's `ed=N` query parameter (anywhere in the query) is WebSocket early data;
/// mihomo takes it as `max-early-data`. Returns the path without it.
fn split_early_data(path: &str) -> (String, Option<u64>) {
    let Some((base, query)) = path.split_once('?') else {
        return (path.to_owned(), None);
    };
    let mut early = None;
    let rest: Vec<&str> = query
        .split('&')
        .filter(
            |param| match param.strip_prefix("ed=").map(str::parse::<u64>) {
                Some(Ok(n)) => {
                    early = Some(n);
                    false
                }
                _ => true,
            },
        )
        .collect();
    let path = if rest.is_empty() {
        base.to_owned()
    } else {
        format!("{base}?{}", rest.join("&"))
    };
    (path, early)
}

fn copy_fields(src: &Json, dst: &mut Mapping, fields: &[(&str, &str, bool)]) {
    for (from, to, stringify) in fields {
        let value = &src[*from];
        if value.is_null() {
            continue;
        }
        let converted = match (stringify, value) {
            (true, Json::String(s)) => Value::from(s.as_str()),
            (true, other) => Value::from(other.to_string()),
            (false, other) => json_to_yaml(other),
        };
        set(dst, to, converted);
    }
}

fn xhttp_opts(xhttp: &Json) -> Mapping {
    let mut opts = Mapping::new();
    for key in ["path", "host", "mode"] {
        if let Some(v) = json_str(&xhttp[key]) {
            set(&mut opts, key, v);
        }
    }
    let extra = &xhttp["extra"];
    let headers = if extra["headers"].is_object() {
        &extra["headers"]
    } else {
        &xhttp["headers"]
    };
    if headers.is_object() {
        set(&mut opts, "headers", json_to_yaml(headers));
    }
    copy_fields(extra, &mut opts, XHTTP_FIELDS);
    if extra["xmux"].is_object() {
        let mut reuse = Mapping::new();
        copy_fields(&extra["xmux"], &mut reuse, XMUX_FIELDS);
        set(&mut opts, "reuse-settings", Value::Mapping(reuse));
    }
    let ds = &extra["downloadSettings"];
    if ds.is_object() {
        let mut download = Mapping::new();
        if let Some(addr) = json_str(&ds["address"]) {
            set(&mut download, "server", addr);
        }
        if let Some(port) = port_of(&ds["port"]) {
            set(&mut download, "port", port);
        }
        let security = ds["security"].as_str().unwrap_or("none");
        if security == "tls" || security == "reality" {
            set(&mut download, "tls", true);
            let tls = &ds["tlsSettings"];
            if let Some(sni) = json_str(&tls["serverName"]) {
                set(&mut download, "servername", sni);
            }
            if let Some(fp) = json_str(&tls["fingerprint"]) {
                set(&mut download, "client-fingerprint", fingerprint(Some(fp)));
            }
            if let Some(alpn) = alpn_list(&tls["alpn"]) {
                set(&mut download, "alpn", alpn);
            }
            if tls["allowInsecure"].as_bool() == Some(true) {
                set(&mut download, "skip-cert-verify", true);
            }
            let reality = &ds["realitySettings"];
            if security == "reality" && reality.is_object() {
                let mut ro = Mapping::new();
                if let Some(pk) = json_str(&reality["publicKey"]) {
                    set(&mut ro, "public-key", pk);
                }
                if let Some(sid) = json_str(&reality["shortId"]) {
                    set(&mut ro, "short-id", sid);
                }
                if !ro.is_empty() {
                    set(&mut download, "reality-opts", Value::Mapping(ro));
                }
            }
        }
        let dx = &ds["xhttpSettings"];
        for key in ["path", "host"] {
            if let Some(v) = json_str(&dx[key]) {
                set(&mut download, key, v);
            }
        }
        if dx["headers"].is_object() {
            set(&mut download, "headers", json_to_yaml(&dx["headers"]));
        }
        if dx["extra"]["xmux"].is_object() {
            let mut reuse = Mapping::new();
            copy_fields(&dx["extra"]["xmux"], &mut reuse, XMUX_FIELDS);
            set(&mut download, "reuse-settings", Value::Mapping(reuse));
        }
        set(&mut opts, "download-settings", Value::Mapping(download));
    }
    opts
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn yaml(v: &Value) -> String {
        serde_norway::to_string(v).unwrap()
    }

    /// Shape produced by Remnawave's XrayJsonGeneratorService (one config per host).
    fn remnawave_list() -> Json {
        json!([
            {
                "remarks": "🇳🇱 NL Reality",
                "outbounds": [
                    {"tag": "proxy", "protocol": "vless",
                     "settings": {"vnext": [{"address": "nl.example.com", "port": 443,
                        "users": [{"id": "11111111-2222-3333-4444-555555555555", "encryption": "none", "flow": "xtls-rprx-vision"}]}]},
                     "streamSettings": {"network": "tcp", "tcpSettings": {}, "security": "reality",
                        "realitySettings": {"serverName": "www.google.com", "publicKey": "PUBKEY", "shortId": "abcd", "fingerprint": "chrome"}}},
                    {"tag": "direct", "protocol": "freedom"},
                    {"tag": "block", "protocol": "blackhole"}
                ],
                "routing": {"rules": []}
            },
            {
                "remarks": "🇩🇪 DE xhttp",
                "outbounds": [
                    {"tag": "proxy", "protocol": "vless",
                     "settings": {"vnext": [{"address": "de.example.com", "port": 443,
                        "users": [{"id": "u2", "encryption": "none", "flow": ""}]}]},
                     "streamSettings": {"network": "xhttp",
                        "xhttpSettings": {"mode": "auto", "host": "cdn.example.com", "path": "/x",
                           "extra": {"xPaddingBytes": "100-1000", "noGRPCHeader": false, "xmux": {"maxConcurrency": "16-32"}}},
                        "security": "tls", "tlsSettings": {"serverName": "cdn.example.com", "fingerprint": "firefox", "alpn": ["h2", "http/1.1"]}}}
                ]
            },
            {
                "remarks": "🇫🇮 FI ws",
                "outbounds": [
                    {"tag": "proxy", "protocol": "trojan",
                     "settings": {"servers": [{"address": "fi.example.com", "port": 8443, "password": "pw"}]},
                     "streamSettings": {"network": "ws", "wsSettings": {"path": "/ws?ed=2048", "host": "fi.example.com"},
                        "security": "tls", "tlsSettings": {"serverName": "fi.example.com"}}}
                ]
            },
            {
                "remarks": "SS",
                "outbounds": [
                    {"tag": "proxy", "protocol": "shadowsocks",
                     "settings": {"servers": [{"address": "1.2.3.4", "port": 8388, "method": "2022-blake3-aes-128-gcm", "password": "k", "uot": true, "UoTVersion": 2}]}}
                ]
            }
        ])
    }

    #[test]
    fn early_data_anywhere_in_the_query() {
        assert_eq!(split_early_data("/ws?ed=2048"), ("/ws".into(), Some(2048)));
        assert_eq!(
            split_early_data("/ws?a=1&ed=2560&b=2"),
            ("/ws?a=1&b=2".into(), Some(2560))
        );
        assert_eq!(split_early_data("/ws?ed=x"), ("/ws?ed=x".into(), None));
        assert_eq!(split_early_data("/plain"), ("/plain".into(), None));
    }

    #[test]
    fn security_edge_cases() {
        let outbound = |stream: Json| {
            json!({"tag": "proxy", "protocol": "vless",
                   "settings": {"vnext": [{"address": "a.example", "port": 443,
                                           "users": [{"id": "u"}]}]},
                   "streamSettings": stream})
        };
        let mut warnings = Vec::new();
        let pin = "AB:".repeat(31) + "AB";
        let node = convert_outbound(
            &outbound(json!({"security": "tls",
                "tlsSettings": {"serverName": "a.example", "pinnedPeerCertSha256": pin}})),
            "n",
            &mut warnings,
        )
        .unwrap()
        .unwrap();
        assert_eq!(node["fingerprint"].as_str(), Some("ab".repeat(32).as_str()));
        assert!(
            node.get("skip-cert-verify").is_none(),
            "a pin never disables verification"
        );

        let err = convert_outbound(
            &outbound(json!({"security": "reality", "realitySettings": {"shortId": "ab"}})),
            "n",
            &mut warnings,
        )
        .unwrap_err();
        assert!(err.contains("publicKey"));

        let ss = json!({"protocol": "shadowsocks",
            "settings": {"servers": [{"address": "s", "port": 1, "method": "aes-128-gcm", "password": "p"}]},
            "streamSettings": {"network": "ws"}});
        assert!(convert_outbound(&ss, "n", &mut warnings).is_err());
    }

    #[test]
    fn remarks_never_shadow_builtin_policies() {
        let doc = json!([{"remarks": "DIRECT", "outbounds": [{"tag": "proxy", "protocol": "trojan",
            "settings": {"servers": [{"address": "t.example", "port": 443, "password": "p"}]}}]}]);
        let converted = convert(&doc).unwrap();
        assert_eq!(converted.proxies[0]["name"].as_str(), Some("DIRECT 2"));
    }

    #[test]
    fn detects_xray_documents() {
        assert!(is_xray(&remnawave_list()));
        assert!(is_xray(&json!({"outbounds": [{"protocol": "vless"}]})));
        assert!(
            !is_xray(&json!({"outbounds": [{"type": "vless"}]})),
            "sing-box"
        );
        assert!(!is_xray(&json!({"a": 1})));
    }

    #[test]
    fn converts_remnawave_list() {
        let c = convert(&remnawave_list()).unwrap();
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        assert_eq!(c.proxies.len(), 4);
        let nl = &c.proxies[0];
        assert_eq!(nl["name"].as_str(), Some("🇳🇱 NL Reality"));
        assert_eq!(nl["type"].as_str(), Some("vless"));
        assert_eq!(nl["flow"].as_str(), Some("xtls-rprx-vision"));
        assert_eq!(nl["servername"].as_str(), Some("www.google.com"));
        assert_eq!(nl["reality-opts"]["public-key"].as_str(), Some("PUBKEY"));
        assert_eq!(nl["network"].as_str(), Some("tcp"));
        assert_eq!(nl["packet-encoding"].as_str(), Some("xudp"));

        let de = &c.proxies[1];
        assert_eq!(de["network"].as_str(), Some("xhttp"), "{}", yaml(de));
        assert_eq!(de["xhttp-opts"]["host"].as_str(), Some("cdn.example.com"));
        assert_eq!(
            de["xhttp-opts"]["x-padding-bytes"].as_str(),
            Some("100-1000")
        );
        assert_eq!(
            de["xhttp-opts"]["reuse-settings"]["max-concurrency"].as_str(),
            Some("16-32")
        );
        assert_eq!(de["client-fingerprint"].as_str(), Some("firefox"));
        assert_eq!(de["alpn"][1].as_str(), Some("http/1.1"));
        assert!(de.get("flow").is_none(), "empty flow dropped");

        let fi = &c.proxies[2];
        assert_eq!(
            fi["sni"].as_str(),
            Some("fi.example.com"),
            "trojan uses sni"
        );
        assert_eq!(fi["ws-opts"]["path"].as_str(), Some("/ws"));
        assert_eq!(fi["ws-opts"]["max-early-data"].as_u64(), Some(2048));
        assert_eq!(
            fi["ws-opts"]["headers"]["Host"].as_str(),
            Some("fi.example.com")
        );

        let ss = &c.proxies[3];
        assert_eq!(ss["type"].as_str(), Some("ss"));
        assert_eq!(ss["cipher"].as_str(), Some("2022-blake3-aes-128-gcm"));
        assert_eq!(ss["udp-over-tcp-version"].as_u64(), Some(2));
    }

    #[test]
    fn single_config_and_flat_settings() {
        let doc = json!({
            "remarks": "ignored with several outbounds",
            "outbounds": [
                {"tag": "a", "protocol": "vless", "settings": {"address": "a.example", "port": "443", "id": "x"},
                 "streamSettings": {"network": "httpupgrade", "httpupgradeSettings": {"path": "/u", "host": "h"}}},
                {"tag": "a", "protocol": "vmess", "settings": {"vnext": [{"address": "b.example", "port": 80, "users": [{"id": "y", "alterId": 0, "security": "auto"}]}]},
                 "streamSettings": {"network": "grpc", "grpcSettings": {"serviceName": "svc"}}},
                {"tag": "direct", "protocol": "freedom"}
            ]
        });
        let c = convert(&doc).unwrap();
        assert_eq!(c.proxies.len(), 2);
        assert_eq!(c.proxies[0]["name"].as_str(), Some("a"));
        assert_eq!(
            c.proxies[1]["name"].as_str(),
            Some("a 2"),
            "names are unique"
        );
        assert_eq!(
            c.proxies[0]["ws-opts"]["v2ray-http-upgrade"].as_bool(),
            Some(true)
        );
        assert_eq!(c.proxies[0]["port"].as_u64(), Some(443));
        assert_eq!(
            c.proxies[1]["grpc-opts"]["grpc-service-name"].as_str(),
            Some("svc")
        );
    }

    #[test]
    fn reports_what_it_cannot_convert() {
        let doc = json!([
            {"remarks": "kcp", "outbounds": [{"tag": "proxy", "protocol": "vless",
              "settings": {"vnext": [{"address": "k.example", "port": 1, "users": [{"id": "z"}]}]},
              "streamSettings": {"network": "kcp"}}]},
            {"remarks": "chained", "outbounds": [{"tag": "proxy", "protocol": "trojan",
              "settings": {"servers": [{"address": "c.example", "port": 443, "password": "p"}]},
              "streamSettings": {"network": "tcp", "sockopt": {"dialerProxy": "fragment"}}}]}
        ]);
        let c = convert(&doc).unwrap();
        assert_eq!(c.proxies.len(), 1);
        assert!(c.warnings.iter().any(|w| w.contains("kcp")));
        assert!(c.warnings.iter().any(|w| w.contains("dialerProxy")));
    }

    #[test]
    fn hysteria_quic_params_are_not_a_mask() {
        // Shape seen in real Happ subscriptions: finalmask carries only QUIC tuning.
        let node = |finalmask: Json| {
            json!([{"remarks": "HY2", "outbounds": [{"tag": "proxy", "protocol": "hysteria",
              "settings": {"address": "h.example", "port": 443, "version": 2},
              "streamSettings": {"network": "hysteria", "security": "tls",
                "hysteriaSettings": {"version": 2, "auth": "secret"},
                "tlsSettings": {"serverName": "h.example", "alpn": ["h3"]},
                "finalmask": finalmask}}]}])
        };
        let quic = json!({"quicParams": {"congestion": "brutal", "brutalUp": "2000 mbps"}});
        let c = convert(&node(quic)).unwrap();
        assert_eq!(c.proxies.len(), 1);
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        let masked = json!({"udp": [{"type": "salamander"}], "quicParams": {}});
        let c = convert(&node(masked)).unwrap();
        assert!(c.warnings.iter().any(|w| w.contains("finalmask")));
    }
}
