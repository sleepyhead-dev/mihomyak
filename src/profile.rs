//! Builds the final mihomo `config.yaml` from subscription content.
//!
//! Layering, lowest priority first (mirrors how FlClashX/Koala patch profiles):
//! 1. the subscription (YAML as-is, or a generated skeleton around share links);
//! 2. host-specific keys stripped: ports, controller, TUN, interfaces — a remote
//!    provider must not decide how this machine is exposed;
//! 3. managed keys from `[core]` and `[gateway]`;
//! 4. `[mihomo]` user overrides, deep-merged;
//! 5. controller and secret re-applied (the CLI depends on them).

use anyhow::{Context, Result};
use serde_norway::{Mapping, Value};

use crate::config::Config;
use crate::emulation::ClientKind;
use crate::subscription::{Content, Format};

/// Provider file for link subscriptions, relative to the mihomo home directory.
pub const PROVIDER_FILE: &str = "providers/subscription.txt";

const HEALTH_CHECK_URL: &str = "https://www.gstatic.com/generate_204";

/// Keys the subscription may not set.
const STRIPPED_KEYS: &[&str] = &[
    "port",
    "socks-port",
    "redir-port",
    "tproxy-port",
    "mixed-port",
    "allow-lan",
    "bind-address",
    "lan-allowed-ips",
    "lan-disallowed-ips",
    "authentication",
    "skip-auth-prefixes",
    "external-controller",
    "external-controller-tls",
    "external-controller-unix",
    "external-controller-pipe",
    "external-controller-cors",
    "external-doh-server",
    "secret",
    "external-ui",
    "external-ui-url",
    "external-ui-name",
    "tun",
    "interface-name",
    "routing-mark",
    "log-level",
    "find-process-mode",
];

/// Skeleton wrapped around share-link subscriptions (mihomo converts the links).
fn links_skeleton() -> Value {
    let yaml = format!(
        r#"
proxy-providers:
  subscription:
    type: file
    path: ./{PROVIDER_FILE}
    health-check: {{ enable: true, url: "{HEALTH_CHECK_URL}", interval: 600, lazy: true }}
proxy-groups:
  - name: PROXY
    type: select
    proxies: [AUTO, DIRECT]
    use: [subscription]
  - name: AUTO
    type: url-test
    use: [subscription]
    url: "{HEALTH_CHECK_URL}"
    interval: 600
    tolerance: 50
    lazy: true
rules:
  - IP-CIDR,127.0.0.0/8,DIRECT,no-resolve
  - IP-CIDR,10.0.0.0/8,DIRECT,no-resolve
  - IP-CIDR,172.16.0.0/12,DIRECT,no-resolve
  - IP-CIDR,192.168.0.0/16,DIRECT,no-resolve
  - IP-CIDR,100.64.0.0/10,DIRECT,no-resolve
  - IP-CIDR6,fc00::/7,DIRECT,no-resolve
  - IP-CIDR6,fe80::/10,DIRECT,no-resolve
  - MATCH,PROXY
"#
    );
    serde_norway::from_str(&yaml).expect("built-in skeleton is valid YAML")
}

pub struct Built {
    pub config_yaml: String,
    /// Contents of [`PROVIDER_FILE`] for link subscriptions.
    pub provider: Option<Vec<u8>>,
}

pub fn build(content: &Content, config: &Config, secret: &str, mode: &str) -> Result<Built> {
    let (mut root, provider) = match content.format {
        Format::Mihomo => (
            content
                .yaml
                .clone()
                .context("mihomo content without YAML")?,
            None,
        ),
        Format::Links => (
            links_skeleton(),
            Some(content.links.clone().unwrap_or_default().into_bytes()),
        ),
    };
    let map = root
        .as_mapping_mut()
        .context("config root is not a mapping")?;
    for key in STRIPPED_KEYS {
        map.remove(*key);
    }
    apply_managed(map, config);
    // Like FlClashX, the client owns the routing mode: Remnawave's default
    // template ships `mode: global`, which would route through GLOBAL → DIRECT.
    set(map, "mode", mode);
    if config.gateway.enable {
        apply_gateway(map, config);
    }
    let overrides = serde_norway::to_value(&config.mihomo).context("convert [mihomo] overrides")?;
    deep_merge(&mut root, overrides);
    let map = root
        .as_mapping_mut()
        .context("[mihomo] overrides replaced the config root")?;
    apply_controller(map, config, secret);
    Ok(Built {
        config_yaml: serde_norway::to_string(&root)?,
        provider,
    })
}

fn apply_managed(map: &mut Mapping, config: &Config) {
    let core = &config.core;
    set(map, "mixed-port", core.mixed_port);
    set(map, "allow-lan", core.allow_lan);
    set(map, "bind-address", core.bind_address.as_str());
    set(map, "log-level", core.log_level.as_str());
    // Process lookup costs CPU on every connection and is useless on a gateway.
    set(map, "find-process-mode", "off");
    // Remember group selections across restarts (mihomo cache.db).
    let profile = child(map, "profile");
    set(profile, "store-selected", true);
    set(profile, "store-fake-ip", true);
    // FlClashX runs mihomo with the default global-ua of its embedded core, so
    // proxy/rule providers are fetched as `clash.meta/<that core version>`.
    if config.subscription.client == ClientKind::FlClashX && !map.contains_key("global-ua") {
        let core_version = config
            .subscription
            .core_version
            .as_deref()
            .unwrap_or(crate::emulation::FLCLASHX_CORE_VERSION);
        set(map, "global-ua", format!("clash.meta/{core_version}"));
    }
}

fn apply_gateway(map: &mut Mapping, config: &Config) {
    let gw = &config.gateway;
    let tun = child(map, "tun");
    set(tun, "enable", true);
    set(tun, "stack", gw.stack.as_str());
    set(tun, "auto-route", true);
    set(tun, "auto-redirect", gw.auto_redirect);
    set(tun, "auto-detect-interface", true);
    set(
        tun,
        "dns-hijack",
        Value::Sequence(vec!["any:53".into(), "tcp://any:53".into()]),
    );

    // Keep the provider's DNS policy, but it must be enabled for hijacked queries.
    let has_dns = map.contains_key("dns");
    let dns = child(map, "dns");
    if !has_dns {
        set(
            dns,
            "nameserver",
            Value::Sequence(vec![
                "https://1.1.1.1/dns-query".into(),
                "https://8.8.8.8/dns-query".into(),
            ]),
        );
        set(
            dns,
            "default-nameserver",
            Value::Sequence(vec!["1.1.1.1".into(), "8.8.8.8".into()]),
        );
    }
    set(dns, "enable", true);
    set(dns, "listen", gw.dns_listen.as_str());
    if !dns.contains_key("enhanced-mode") {
        set(dns, "enhanced-mode", "fake-ip");
        set(dns, "fake-ip-range", "198.18.0.1/16");
    }
}

fn apply_controller(map: &mut Mapping, config: &Config, secret: &str) {
    map.remove("external-controller");
    map.remove("external-controller-unix");
    match config.core.controller.strip_prefix("unix:") {
        Some(path) => set(map, "external-controller-unix", path),
        None => set(map, "external-controller", config.core.controller.as_str()),
    }
    set(map, "secret", secret);
}

fn set(map: &mut Mapping, key: &str, value: impl Into<Value>) {
    map.insert(Value::from(key), value.into());
}

/// Returns `map[key]` as a mapping, replacing any non-mapping value.
fn child<'a>(map: &'a mut Mapping, key: &str) -> &'a mut Mapping {
    let entry = map
        .entry(Value::from(key))
        .or_insert_with(|| Value::Mapping(Mapping::new()));
    if !entry.is_mapping() {
        *entry = Value::Mapping(Mapping::new());
    }
    entry.as_mapping_mut().expect("just ensured a mapping")
}

/// Recursively merges mappings; any other value in `overlay` replaces the base.
pub fn deep_merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Mapping(base_map), Value::Mapping(overlay_map)) => {
            for (key, value) in overlay_map {
                match base_map.get_mut(&key) {
                    Some(existing) => deep_merge(existing, value),
                    None => {
                        base_map.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subscription::body;

    fn parsed(yaml: &str) -> Content {
        body::parse(yaml.as_bytes(), None).unwrap()
    }

    fn built(content: &Content, config: &Config) -> Value {
        serde_norway::from_str(
            &build(content, config, "s3cret", "rule")
                .unwrap()
                .config_yaml,
        )
        .unwrap()
    }

    const REMNAWAVE: &str = r#"
mixed-port: 7890
socks-port: 7891
allow-lan: false
mode: global
log-level: info
external-controller: 0.0.0.0:9090
secret: ""
interface-name: en0
tun: {enable: true, stack: gvisor}
dns: {enable: true, enhanced-mode: redir-host, nameserver: [1.1.1.1]}
proxies:
  - {name: NL, type: vless, server: nl.example.com, port: 443, uuid: x}
proxy-groups:
  - {name: '→ Remnawave', type: select, proxies: [NL]}
rules:
  - MATCH,→ Remnawave
"#;

    #[test]
    fn strips_provider_networking_keys() {
        let v = built(&parsed(REMNAWAVE), &Config::default());
        assert_eq!(v["mixed-port"].as_u64(), Some(7890));
        assert!(v.get("socks-port").is_none());
        assert!(v.get("interface-name").is_none());
        assert!(v.get("tun").is_none());
        assert_eq!(v["allow-lan"].as_bool(), Some(true));
        assert_eq!(v["log-level"].as_str(), Some("warning"));
        assert_eq!(v["external-controller"].as_str(), Some("127.0.0.1:9090"));
        assert_eq!(v["secret"].as_str(), Some("s3cret"));
        assert_eq!(v["mode"].as_str(), Some("rule"), "the client owns the mode");
        assert_eq!(v["dns"]["enhanced-mode"].as_str(), Some("redir-host"));
        assert_eq!(v["profile"]["store-selected"].as_bool(), Some(true));
        assert_eq!(v["global-ua"].as_str(), Some("clash.meta/v1.19.28"));
        assert_eq!(v["proxies"][0]["name"].as_str(), Some("NL"));
    }

    #[test]
    fn gateway_enables_tun_and_dns() {
        let mut config = Config::default();
        config.gateway.enable = true;
        let v = built(&parsed(REMNAWAVE), &config);
        assert_eq!(v["tun"]["enable"].as_bool(), Some(true));
        assert_eq!(v["tun"]["stack"].as_str(), Some("system"));
        assert_eq!(v["tun"]["auto-route"].as_bool(), Some(true));
        assert_eq!(v["dns"]["listen"].as_str(), Some("0.0.0.0:1053"));
        assert_eq!(
            v["dns"]["nameserver"][0].as_str(),
            Some("1.1.1.1"),
            "provider DNS kept"
        );

        let v = built(
            &parsed("proxies: [{name: a, type: ss, server: h, port: 1080}]"),
            &config,
        );
        assert_eq!(v["dns"]["enhanced-mode"].as_str(), Some("fake-ip"));
        assert_eq!(v["dns"]["enable"].as_bool(), Some(true));
    }

    #[test]
    fn user_overrides_win_but_not_over_controller() {
        let mut config: Config = toml::from_str(
            r#"
            [core]
            controller = "unix:/data/mihomo.sock"
            [mihomo]
            mode = "rule"
            secret = "hijack"
            dns = { ipv6 = true }
            profile = { store-selected = false }
            "#,
        )
        .unwrap();
        config.subscription.client = ClientKind::Koala;
        let v = built(&parsed(REMNAWAVE), &config);
        assert_eq!(v["mode"].as_str(), Some("rule"));
        assert_eq!(v["dns"]["ipv6"].as_bool(), Some(true));
        assert_eq!(v["dns"]["enhanced-mode"].as_str(), Some("redir-host"));
        assert_eq!(v["profile"]["store-selected"].as_bool(), Some(false));
        assert_eq!(v["secret"].as_str(), Some("s3cret"));
        assert_eq!(
            v["external-controller-unix"].as_str(),
            Some("/data/mihomo.sock")
        );
        assert!(v.get("external-controller").is_none());
        assert!(v.get("global-ua").is_none(), "only FlClashX pins global-ua");
    }

    #[test]
    fn wraps_share_links() {
        let content = body::parse(b"trojan://p@h.example:443#TR\n", None).unwrap();
        let built = build(&content, &Config::default(), "x", "rule").unwrap();
        assert_eq!(
            built.provider.as_deref(),
            Some(&b"trojan://p@h.example:443#TR\n"[..])
        );
        let v: Value = serde_norway::from_str(&built.config_yaml).unwrap();
        assert_eq!(
            v["proxy-providers"]["subscription"]["path"].as_str(),
            Some("./providers/subscription.txt")
        );
        assert_eq!(
            v["rules"].as_sequence().unwrap().last().unwrap().as_str(),
            Some("MATCH,PROXY")
        );
    }

    #[test]
    fn deep_merge_semantics() {
        let mut base: Value = serde_norway::from_str("a: {b: 1, c: [1, 2]}\nd: x").unwrap();
        let overlay: Value = serde_norway::from_str("a: {c: [3], e: 4}\nd: {f: 1}").unwrap();
        deep_merge(&mut base, overlay);
        let expected: Value = serde_norway::from_str("a: {b: 1, c: [3], e: 4}\nd: {f: 1}").unwrap();
        assert_eq!(base, expected);
    }
}
