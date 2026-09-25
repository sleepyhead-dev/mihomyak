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

use crate::config::{Config, GroupType, Preset};
use crate::emulation::ClientKind;
use crate::pattern::{PatternSet, keep};
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

/// Rules used when the subscription brings none: LAN direct, everything else proxied.
const DEFAULT_RULES: &[&str] = &[
    "IP-CIDR,127.0.0.0/8,DIRECT,no-resolve",
    "IP-CIDR,10.0.0.0/8,DIRECT,no-resolve",
    "IP-CIDR,172.16.0.0/12,DIRECT,no-resolve",
    "IP-CIDR,192.168.0.0/16,DIRECT,no-resolve",
    "IP-CIDR,100.64.0.0/10,DIRECT,no-resolve",
    "IP-CIDR6,fc00::/7,DIRECT,no-resolve",
    "IP-CIDR6,fe80::/10,DIRECT,no-resolve",
    "MATCH,PROXY",
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
"#
    );
    serde_norway::from_str(&yaml).expect("built-in skeleton is valid YAML")
}

/// Gives configs that only list proxies (link lists, Xray JSON, bare `proxies:`
/// subscriptions) a `PROXY` selector, an `AUTO` url-test group and default rules.
fn ensure_groups(map: &mut Mapping) {
    let has_groups = map
        .get("proxy-groups")
        .and_then(Value::as_sequence)
        .is_some_and(|g| !g.is_empty());
    if has_groups {
        return;
    }
    let names: Vec<Value> = map
        .get("proxies")
        .and_then(Value::as_sequence)
        .into_iter()
        .flatten()
        .filter_map(|p| p.get("name").cloned())
        .collect();
    let providers: Vec<Value> = map
        .get("proxy-providers")
        .and_then(Value::as_mapping)
        .into_iter()
        .flat_map(|m| m.keys().cloned())
        .collect();
    let group = |name: &str, kind: &str, proxies: Vec<Value>| {
        let mut g = Mapping::new();
        set(&mut g, "name", name);
        set(&mut g, "type", kind);
        set(&mut g, "proxies", Value::Sequence(proxies));
        if !providers.is_empty() {
            set(&mut g, "use", Value::Sequence(providers.clone()));
        }
        if kind == "url-test" {
            set(&mut g, "url", HEALTH_CHECK_URL);
            set(&mut g, "interval", 600);
            set(&mut g, "tolerance", 50);
            set(&mut g, "lazy", true);
        }
        Value::Mapping(g)
    };
    let mut select = vec![Value::from("AUTO")];
    select.extend(names.iter().cloned());
    select.push(Value::from("DIRECT"));
    let groups = vec![
        group("PROXY", "select", select),
        group("AUTO", "url-test", names),
    ];
    set(map, "proxy-groups", Value::Sequence(groups));
    // Provider rules would reference groups that do not exist; ours route via PROXY.
    let rules = DEFAULT_RULES.iter().map(|r| Value::from(*r)).collect();
    set(map, "rules", Value::Sequence(rules));
}

pub struct Built {
    pub config_yaml: String,
    /// Contents of [`PROVIDER_FILE`] for link subscriptions.
    pub provider: Option<Vec<u8>>,
    /// Things the user should know about (filters that emptied groups, …).
    pub warnings: Vec<String>,
}

pub fn build(content: &Content, config: &Config, secret: &str, mode: &str) -> Result<Built> {
    let (mut root, provider) = match content.format {
        Format::Mihomo | Format::XrayJson => (
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
    let mut warnings = Vec::new();
    apply_filter(map, config, &mut warnings);
    ensure_groups(map);
    apply_custom_groups(map, config, &mut warnings);
    let mut extra_rules = config.rules.prepend.clone();
    for preset in &config.rules.presets {
        extra_rules.extend(preset_rules(*preset).iter().map(|r| r.to_string()));
    }
    prepend_rules(map, &extra_rules);
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
        warnings,
    })
}

fn name_of(proxy: &Value) -> Option<String> {
    proxy.get("name").and_then(Value::as_str).map(str::to_owned)
}

fn inline_names(map: &Mapping) -> Vec<String> {
    map.get("proxies")
        .and_then(Value::as_sequence)
        .into_iter()
        .flatten()
        .filter_map(name_of)
        .collect()
}

fn provider_names(map: &Mapping) -> Vec<Value> {
    map.get("proxy-providers")
        .and_then(Value::as_mapping)
        .into_iter()
        .flat_map(|m| m.keys().cloned())
        .collect()
}

/// `[filter]`: drops nodes from `proxies` and every group, and constrains
/// proxy-providers (mihomo `filter` / `exclude-filter`) the same way.
fn apply_filter(map: &mut Mapping, config: &Config, warnings: &mut Vec<String>) {
    let include = PatternSet::new(&config.filter.include);
    let exclude = PatternSet::new(&config.filter.exclude);
    if include.is_empty() && exclude.is_empty() {
        return;
    }
    let mut removed = std::collections::HashSet::new();
    if let Some(proxies) = map.get_mut("proxies").and_then(Value::as_sequence_mut) {
        proxies.retain(|p| match name_of(p) {
            Some(name) if !keep(&name, &include, &exclude) => {
                removed.insert(name);
                false
            }
            _ => true,
        });
    }
    let mut has_providers = false;
    if let Some(providers) = map
        .get_mut("proxy-providers")
        .and_then(Value::as_mapping_mut)
    {
        for (name, provider) in providers.iter_mut() {
            let Some(provider) = provider.as_mapping_mut() else {
                continue;
            };
            has_providers = true;
            for (key, set_) in [("filter", &include), ("exclude-filter", &exclude)] {
                if set_.is_empty() {
                    continue;
                }
                if provider.contains_key(key) {
                    warnings.push(format!(
                        "provider {} already has a {key}; replaced by [filter]",
                        name.as_str().unwrap_or("?")
                    ));
                }
                set(provider, key, set_.to_regex());
            }
        }
    }
    if let Some(groups) = map.get_mut("proxy-groups").and_then(Value::as_sequence_mut) {
        for group in groups.iter_mut().filter_map(Value::as_mapping_mut) {
            let uses_providers = group.contains_key("use") || group.contains_key("include-all");
            let Some(members) = group.get_mut("proxies").and_then(Value::as_sequence_mut) else {
                continue;
            };
            members.retain(|m| m.as_str().is_none_or(|n| !removed.contains(n)));
            if members.is_empty() && !uses_providers {
                // Keep references to the group valid.
                members.push(Value::from("DIRECT"));
                let name = group.get("name").and_then(Value::as_str).unwrap_or("?");
                warnings.push(format!(
                    "[filter] left group {name:?} without nodes (DIRECT)"
                ));
            }
        }
    }
    if !removed.is_empty() {
        crate::debug!("[filter] dropped {} node(s)", removed.len());
    }
    if inline_names(map).is_empty() && !has_providers {
        warnings.push("[filter] removed every node of the subscription".into());
    }
}

/// `[[groups]]`: user-defined auto-switching groups, listed first.
fn apply_custom_groups(map: &mut Mapping, config: &Config, warnings: &mut Vec<String>) {
    if config.groups.is_empty() {
        return;
    }
    let names = inline_names(map);
    let providers = provider_names(map);
    let mut built = Vec::with_capacity(config.groups.len());
    let mut defaults = Vec::new();
    for g in &config.groups {
        let patterns = PatternSet::new(&g.nodes);
        let mut members: Vec<String> = Vec::new();
        if patterns.is_empty() {
            members.clone_from(&names);
        } else {
            // Pattern order is priority order (matters for fallback).
            for pattern in patterns.patterns() {
                for name in names.iter().filter(|n| pattern.matches(n)) {
                    if !members.contains(name) {
                        members.push(name.clone());
                    }
                }
            }
        }
        let mut group = Mapping::new();
        set(&mut group, "name", g.name.as_str());
        set(&mut group, "type", g.kind.as_mihomo());
        if !providers.is_empty() {
            set(&mut group, "use", Value::Sequence(providers.clone()));
            if !patterns.is_empty() {
                set(&mut group, "filter", patterns.to_regex());
            }
        } else if members.is_empty() {
            warnings.push(format!("group {:?}: no node matches {:?}", g.name, g.nodes));
            members.push("DIRECT".into());
        }
        set(
            &mut group,
            "proxies",
            Value::Sequence(members.iter().map(|m| Value::from(m.as_str())).collect()),
        );
        if g.kind != GroupType::Select {
            set(&mut group, "url", g.url.as_str());
            set(&mut group, "interval", g.interval.0.as_secs());
            // Failover needs live health data even while the group is idle.
            set(&mut group, "lazy", false);
        }
        if g.kind == GroupType::UrlTest {
            set(&mut group, "tolerance", g.tolerance);
        }
        built.push(Value::Mapping(group));
        if g.default {
            defaults.push((g.name.clone(), members));
        }
    }
    let groups = map
        .entry(Value::from("proxy-groups"))
        .or_insert_with(|| Value::Sequence(Vec::new()));
    let Some(groups) = groups.as_sequence_mut() else {
        return;
    };
    // Offer default groups first in every selector that holds their nodes, so a
    // fresh start picks them (mihomo selects the first member).
    for existing in groups.iter_mut().filter_map(Value::as_mapping_mut) {
        if existing.get("type").and_then(Value::as_str) != Some("select") {
            continue;
        }
        let uses_providers = existing.contains_key("use");
        let Some(members) = existing.get_mut("proxies").and_then(Value::as_sequence_mut) else {
            continue;
        };
        for (name, nodes) in defaults.iter().rev() {
            let relevant = uses_providers
                || members
                    .iter()
                    .any(|m| m.as_str().is_some_and(|m| nodes.iter().any(|n| n == m)));
            if relevant && !members.iter().any(|m| m.as_str() == Some(name.as_str())) {
                members.insert(0, Value::from(name.as_str()));
            }
        }
    }
    groups.splice(0..0, built);
}

/// Built-in rule sets. GEOIP uses `no-resolve`: resolving every domain just to
/// test its country would leak all DNS queries to the local resolver.
fn preset_rules(preset: Preset) -> &'static [&'static str] {
    match preset {
        Preset::RuDirect => &[
            "DOMAIN-SUFFIX,ru,DIRECT",
            "DOMAIN-SUFFIX,su,DIRECT",
            "DOMAIN-SUFFIX,xn--p1ai,DIRECT",
            "GEOSITE,category-ru,DIRECT",
            "GEOIP,ru,DIRECT,no-resolve",
        ],
    }
}

fn prepend_rules(map: &mut Mapping, extra: &[String]) {
    if extra.is_empty() {
        return;
    }
    let rules = map
        .entry(Value::from("rules"))
        .or_insert_with(|| Value::Sequence(Vec::new()));
    if let Some(rules) = rules.as_sequence_mut() {
        rules.splice(0..0, extra.iter().map(|r| Value::from(r.as_str())));
    }
}

fn apply_managed(map: &mut Mapping, config: &Config) {
    let core = &config.core;
    set(map, "mixed-port", core.mixed_port);
    set(map, "allow-lan", core.allow_lan);
    set(map, "bind-address", core.bind_address.as_str());
    let seq =
        |items: &[String]| Value::Sequence(items.iter().map(|s| Value::from(s.as_str())).collect());
    if core.allow_lan {
        set(map, "lan-allowed-ips", seq(&core.lan_allowed_ips));
    }
    if !core.auth.is_empty() {
        set(map, "authentication", seq(&core.auth));
        // Local processes and the TUN gateway path never need credentials.
        set(
            map,
            "skip-auth-prefixes",
            Value::Sequence(vec!["127.0.0.1/8".into(), "::1/128".into()]),
        );
    }
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
        assert_eq!(v["allow-lan"].as_bool(), Some(false), "secure default");
        assert!(v.get("lan-allowed-ips").is_none());
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
        assert_eq!(v["dns"]["listen"].as_str(), Some("127.0.0.1:1053"));
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
    fn adds_groups_to_bare_proxy_lists() {
        let content = parsed(
            "proxies:\n  - {name: A, type: ss, server: a.example, port: 1080, cipher: aes-128-gcm, password: p}\nrules: [MATCH,DIRECT]\n",
        );
        let v = built(&content, &Config::default());
        assert_eq!(v["proxy-groups"][0]["name"].as_str(), Some("PROXY"));
        let members: Vec<_> = v["proxy-groups"][0]["proxies"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|m| m.as_str().unwrap())
            .collect();
        assert_eq!(members, ["AUTO", "A", "DIRECT"]);
        assert_eq!(v["proxy-groups"][1]["proxies"][0].as_str(), Some("A"));
        assert_eq!(
            v["rules"].as_sequence().unwrap().last().unwrap().as_str(),
            Some("MATCH,PROXY")
        );

        // A provider's own groups and rules are left alone.
        let v = built(&parsed(REMNAWAVE), &Config::default());
        assert_eq!(v["proxy-groups"].as_sequence().unwrap().len(), 1);
        assert_eq!(v["rules"][0].as_str(), Some("MATCH,→ Remnawave"));
    }

    const THREE_NODES: &str = r#"
proxies:
  - {name: "🇳🇱 NL 1", type: ss, server: nl1.example, port: 1, cipher: aes-128-gcm, password: p}
  - {name: "🇩🇪 DE 1", type: ss, server: de1.example, port: 1, cipher: aes-128-gcm, password: p}
  - {name: "🇷🇺 RU info", type: ss, server: ru.example, port: 1, cipher: aes-128-gcm, password: p}
proxy-groups:
  - {name: Main, type: select, proxies: ["🇳🇱 NL 1", "🇩🇪 DE 1", "🇷🇺 RU info"]}
  - {name: RU only, type: select, proxies: ["🇷🇺 RU info"]}
rules: ["MATCH,Main"]
"#;

    fn names(v: &Value) -> Vec<String> {
        v.as_sequence()
            .unwrap()
            .iter()
            .map(|p| {
                p.as_str()
                    .or_else(|| p["name"].as_str())
                    .unwrap()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn filter_drops_nodes_everywhere() {
        let config: Config = toml::from_str("[filter]\nexclude = [\"*ru*\"]").unwrap();
        let built = build(&parsed(THREE_NODES), &config, "s", "rule").unwrap();
        let v: Value = serde_norway::from_str(&built.config_yaml).unwrap();
        assert_eq!(names(&v["proxies"]), ["🇳🇱 NL 1", "🇩🇪 DE 1"]);
        assert_eq!(
            names(&v["proxy-groups"][0]["proxies"]),
            ["🇳🇱 NL 1", "🇩🇪 DE 1"]
        );
        assert_eq!(names(&v["proxy-groups"][1]["proxies"]), ["DIRECT"]);
        assert!(built.warnings[0].contains("RU only"));

        let config: Config = toml::from_str("[filter]\ninclude = [\"*nl*\"]").unwrap();
        let v = built_v(THREE_NODES, &config);
        assert_eq!(names(&v["proxies"]), ["🇳🇱 NL 1"]);
    }

    fn built_v(yaml: &str, config: &Config) -> Value {
        serde_norway::from_str(
            &build(&parsed(yaml), config, "s", "rule")
                .unwrap()
                .config_yaml,
        )
        .unwrap()
    }

    #[test]
    fn filter_constrains_link_providers() {
        let config: Config =
            toml::from_str("[filter]\ninclude = [\"*NL*\"]\nexclude = [\"*(test)*\"]").unwrap();
        let content = body::parse(b"trojan://p@h.example:443#NL\n", None).unwrap();
        let v: Value =
            serde_norway::from_str(&build(&content, &config, "s", "rule").unwrap().config_yaml)
                .unwrap();
        let provider = &v["proxy-providers"]["subscription"];
        assert_eq!(provider["filter"].as_str(), Some("(?i)^(?:.*NL.*)$"));
        assert_eq!(
            provider["exclude-filter"].as_str(),
            Some(r"(?i)^(?:.*\(test\).*)$")
        );
    }

    #[test]
    fn custom_auto_group_becomes_default() {
        let config: Config = toml::from_str(
            r#"
            [[groups]]
            name = "Auto"
            type = "fallback"
            nodes = ["*DE*", "*NL*"]
            default = true
            [rules]
            prepend = ["DOMAIN-SUFFIX,lan,DIRECT"]
            "#,
        )
        .unwrap();
        let v = built_v(THREE_NODES, &config);
        let auto = &v["proxy-groups"][0];
        assert_eq!(auto["name"].as_str(), Some("Auto"));
        assert_eq!(auto["type"].as_str(), Some("fallback"));
        assert_eq!(
            names(&auto["proxies"]),
            ["🇩🇪 DE 1", "🇳🇱 NL 1"],
            "pattern order = priority"
        );
        assert_eq!(auto["interval"].as_u64(), Some(180));
        assert_eq!(auto["lazy"].as_bool(), Some(false));
        let main = &v["proxy-groups"][1];
        assert_eq!(names(&main["proxies"])[0], "Auto");
        assert_eq!(
            names(&v["proxy-groups"][2]["proxies"]),
            ["🇷🇺 RU info"],
            "unrelated selector untouched"
        );
        assert_eq!(v["rules"][0].as_str(), Some("DOMAIN-SUFFIX,lan,DIRECT"));
        assert_eq!(v["rules"][1].as_str(), Some("MATCH,Main"));
    }

    #[test]
    fn custom_group_over_link_provider() {
        let config: Config = toml::from_str(
            "[[groups]]\nname = \"Fast\"\ntype = \"url-test\"\nnodes = [\"*NL*\"]\ndefault = true",
        )
        .unwrap();
        let content = body::parse(b"trojan://p@h.example:443#NL\n", None).unwrap();
        let v: Value =
            serde_norway::from_str(&build(&content, &config, "s", "rule").unwrap().config_yaml)
                .unwrap();
        let fast = &v["proxy-groups"][0];
        assert_eq!(fast["use"][0].as_str(), Some("subscription"));
        assert_eq!(fast["filter"].as_str(), Some("(?i)^(?:.*NL.*)$"));
        assert_eq!(fast["tolerance"].as_u64(), Some(50));
        assert_eq!(v["proxy-groups"][1]["proxies"][0].as_str(), Some("Fast"));
    }

    #[test]
    fn lan_exposure_and_auth() {
        let config: Config = toml::from_str(
            "[core]\nallow_lan = true\nauth = [\"me:s3cret\"]\n[rules]\npresets = [\"ru-direct\"]\nprepend = [\"DOMAIN,a.example,DIRECT\"]",
        )
        .unwrap();
        let v = built(&parsed(REMNAWAVE), &config);
        assert_eq!(v["allow-lan"].as_bool(), Some(true));
        assert_eq!(v["lan-allowed-ips"][0].as_str(), Some("10.0.0.0/8"));
        assert_eq!(v["authentication"][0].as_str(), Some("me:s3cret"));
        assert_eq!(v["skip-auth-prefixes"][0].as_str(), Some("127.0.0.1/8"));
        assert_eq!(v["rules"][0].as_str(), Some("DOMAIN,a.example,DIRECT"));
        assert_eq!(v["rules"][1].as_str(), Some("DOMAIN-SUFFIX,ru,DIRECT"));
        assert_eq!(v["rules"][5].as_str(), Some("GEOIP,ru,DIRECT,no-resolve"));
        assert_eq!(v["rules"][6].as_str(), Some("MATCH,→ Remnawave"));
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
