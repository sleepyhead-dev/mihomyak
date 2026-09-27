//! Builds the final mihomo `config.yaml` from subscription content.
//!
//! Layering, lowest priority first (mirrors how FlClashX/Koala patch profiles):
//! 1. the subscription, reduced to an **allowlist** of keys (proxies, groups,
//!    providers, rules, DNS policy): a remote provider must never open listeners,
//!    tunnels or ports on this host, touch its clock, or swap geodata sources;
//! 2. node sanitising (unsupported proxy types, `[filter]`) with every reference
//!    to a dropped node rewritten, so mihomo never sees a dangling name;
//! 3. default groups, `[[groups]]`, `[rules]`;
//! 4. managed keys from `[core]` and `[gateway]`;
//! 5. `[mihomo]` user overrides, deep-merged;
//! 6. controller and secret re-applied (the CLI depends on them).

use std::collections::HashSet;
use std::net::IpAddr;

use anyhow::{Context, Result, bail};
use serde_norway::{Mapping, Value};

use crate::client::emulation::ClientKind;
use crate::config::{Config, GroupType, Preset};
use crate::subscription::{Content, Format};
use crate::util::pattern::{PatternSet, keep};

/// Provider file for link subscriptions, relative to the mihomo home directory.
pub const PROVIDER_FILE: &str = "providers/subscription.txt";

/// URL used for latency checks everywhere (groups, CLI, TUI).
pub const HEALTH_CHECK_URL: &str = "https://www.gstatic.com/generate_204";

/// Top-level keys taken from the subscription; everything else is dropped.
const PROVIDER_KEYS: &[&str] = &[
    "proxies",
    "proxy-groups",
    "proxy-providers",
    "rule-providers",
    "rules",
    "sub-rules",
    "dns",
    "sniffer",
    "ipv6",
    "unified-delay",
    "tcp-concurrent",
    "keep-alive-interval",
    "keep-alive-idle",
    "disable-keep-alive",
    "global-client-fingerprint",
    "global-ua",
    "geodata-mode",
    "geodata-loader",
    "geosite-matcher",
];

/// `dns` keys a provider may set: resolution policy only, never `listen`.
const PROVIDER_DNS_KEYS: &[&str] = &[
    "enable",
    "prefer-h3",
    "ipv6",
    "ipv6-timeout",
    "use-hosts",
    "use-system-hosts",
    "respect-rules",
    "enhanced-mode",
    "fake-ip-range",
    "fake-ip-range6",
    "fake-ip-filter",
    "fake-ip-filter-mode",
    "fake-ip-ttl",
    "default-nameserver",
    "nameserver",
    "fallback",
    "fallback-filter",
    "proxy-server-nameserver",
    "direct-nameserver",
    "direct-nameserver-follow-policy",
    "nameserver-policy",
    "cache-algorithm",
    "cache-max-size",
];

/// Outbound proxy types accepted from a subscription. Anything else (notably
/// overlay-network types such as tailscale/zerotier/easytier, which would join
/// this host to a network the provider controls) is dropped with a warning.
const PROXY_TYPES: &[&str] = &[
    "ss",
    "ssr",
    "vmess",
    "vless",
    "trojan",
    "hysteria",
    "hysteria2",
    "tuic",
    "socks5",
    "http",
    "snell",
    "anytls",
    "wireguard",
    "mieru",
    "ssh",
    "direct",
];

/// The same protection for proxies mihomo loads itself from providers.
const PROVIDER_EXCLUDED_TYPES: &str = "tailscale|zerotier|easytier|openvpn|dhcp";

/// Names mihomo reserves for built-in proxies.
pub const BUILTIN_NAMES: &[&str] = &[
    "DIRECT",
    "REJECT",
    "REJECT-DROP",
    "PASS",
    "COMPATIBLE",
    "GLOBAL",
];

/// LAN destinations that stay direct when mihomyak writes the rules itself.
const LAN_RULES: &[&str] = &[
    "IP-CIDR,127.0.0.0/8,DIRECT,no-resolve",
    "IP-CIDR,10.0.0.0/8,DIRECT,no-resolve",
    "IP-CIDR,172.16.0.0/12,DIRECT,no-resolve",
    "IP-CIDR,192.168.0.0/16,DIRECT,no-resolve",
    "IP-CIDR,100.64.0.0/10,DIRECT,no-resolve",
    "IP-CIDR6,fc00::/7,DIRECT,no-resolve",
    "IP-CIDR6,fe80::/10,DIRECT,no-resolve",
];

/// Runtime inputs besides the settings.
pub struct Params<'a> {
    pub secret: &'a str,
    pub mode: &'a str,
    /// Subscription hosts and their last-seen addresses. In gateway mode the
    /// supervisor shares mihomo's network namespace; these keep its own fetches
    /// out of the tunnel (real DNS answers + TUN route exclusions), so dead
    /// nodes never prevent fetching fresh ones.
    pub panel_hosts: &'a [String],
    pub panel_ips: &'a [IpAddr],
}

pub struct Built {
    pub config_yaml: String,
    /// Contents of [`PROVIDER_FILE`] for link subscriptions.
    pub provider: Option<Vec<u8>>,
    /// Things the user should know about (dropped nodes, emptied groups, …).
    pub warnings: Vec<String>,
}

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

pub fn build(content: &Content, config: &Config, params: &Params<'_>) -> Result<Built> {
    let (source, provider) = match content.format {
        Format::Mihomo | Format::XrayJson => (
            content
                .yaml
                .as_ref()
                .context("mihomo content without YAML")?,
            None,
        ),
        Format::Links => (
            &links_skeleton(),
            Some(content.links.clone().unwrap_or_default().into_bytes()),
        ),
    };
    let source = source
        .as_mapping()
        .context("config root is not a mapping")?;
    let mut warnings = Vec::new();
    let mut map = take_provider_keys(source, &mut warnings);
    if content.format != Format::Links {
        // The links skeleton is ours; its file provider is the point.
        confine_providers(&mut map, &mut warnings);
    }
    sanitize_nodes(&mut map, config, &mut warnings);
    ensure_groups(&mut map);
    apply_custom_groups(&mut map, config, &mut warnings)?;
    let mut extra_rules = config.rules.prepend.clone();
    for preset in &config.rules.presets {
        extra_rules.extend(preset_rules(*preset).iter().map(|r| r.to_string()));
    }
    prepend_rules(&mut map, &extra_rules);
    apply_managed(&mut map, config);
    // Like FlClashX, the client owns the routing mode: Remnawave's default
    // template ships `mode: global`, which would route through GLOBAL → DIRECT.
    set(&mut map, "mode", params.mode);
    if config.gateway.enable {
        apply_gateway(&mut map, config, params);
    }
    let mut root = Value::Mapping(map);
    let overrides = serde_norway::to_value(&config.mihomo).context("convert [mihomo] overrides")?;
    deep_merge(&mut root, overrides);
    let map = root
        .as_mapping_mut()
        .context("[mihomo] overrides replaced the config root")?;
    apply_controller(map, config, params.secret);
    if config.gateway.enable && config.gateway.kill_switch {
        // The kill switch lets out only this TUN device and marked traffic, so
        // `[mihomo]` must not rename one or unmark the other.
        set(map, "routing-mark", crate::gateway::killswitch::MARK);
        set(
            child(map, "tun"),
            "device",
            crate::gateway::killswitch::TUN_DEVICE,
        );
    }
    Ok(Built {
        config_yaml: serde_norway::to_string(&root)?,
        provider,
        warnings,
    })
}

/// Copies only [`PROVIDER_KEYS`] (and sanitised `dns`) out of the subscription.
fn take_provider_keys(source: &Mapping, warnings: &mut Vec<String>) -> Mapping {
    let mut map = Mapping::new();
    let mut dropped = Vec::new();
    for (key, value) in source {
        let Some(name) = key.as_str() else { continue };
        if !PROVIDER_KEYS.contains(&name) {
            dropped.push(name.to_owned());
            continue;
        }
        let value = if name == "dns" {
            let mut dns = Mapping::new();
            for (k, v) in value.as_mapping().into_iter().flatten() {
                if k.as_str().is_some_and(|k| PROVIDER_DNS_KEYS.contains(&k)) {
                    dns.insert(k.clone(), v.clone());
                }
            }
            Value::Mapping(dns)
        } else {
            value.clone()
        };
        map.insert(key.clone(), value);
    }
    // Harmless client settings are expected in every template; only surprising
    // keys are worth a warning.
    const EXPECTED: &[&str] = &[
        "port",
        "socks-port",
        "redir-port",
        "tproxy-port",
        "mixed-port",
        "allow-lan",
        "bind-address",
        "mode",
        "log-level",
        "external-controller",
        "secret",
        "tun",
        "profile",
        "find-process-mode",
        "external-ui",
        "external-ui-url",
        "hosts",
        "geox-url",
        "geo-auto-update",
        "geo-update-interval",
        "interface-name",
    ];
    let unexpected: Vec<&String> = dropped
        .iter()
        .filter(|k| !EXPECTED.contains(&k.as_str()))
        .collect();
    if !unexpected.is_empty() {
        warnings.push(format!(
            "ignored subscription keys a provider may not set: {}",
            unexpected
                .iter()
                .map(|k| k.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    map
}

/// Providers write their downloads into mihomo's home directory. A subscription
/// that picks `path` could overwrite config.yaml or cache.db there, and a `file`
/// provider would read local files. Remote (`http`) providers therefore get
/// mihomo's default hashed path, `inline` ones stay, anything else is dropped
/// together with every reference to it.
fn confine_providers(map: &mut Mapping, warnings: &mut Vec<String>) {
    let mut dropped: [HashSet<String>; 2] = Default::default();
    for (key, dropped) in ["proxy-providers", "rule-providers"]
        .iter()
        .zip(&mut dropped)
    {
        let Some(providers) = map.get_mut(*key).and_then(Value::as_mapping_mut) else {
            continue;
        };
        providers.retain(|name, provider| {
            let kind = provider
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            match (kind.as_str(), provider.as_mapping_mut()) {
                ("http", Some(provider)) => {
                    provider.remove("path");
                    true
                }
                ("inline", Some(_)) => true,
                _ => {
                    dropped.insert(name.as_str().unwrap_or_default().to_owned());
                    false
                }
            }
        });
    }
    let [proxy_providers, rule_providers] = dropped;
    if !proxy_providers.is_empty() {
        warnings.push(format!(
            "dropped local proxy-providers: {}",
            sorted(&proxy_providers)
        ));
        let groups = map.get_mut("proxy-groups").and_then(Value::as_sequence_mut);
        for group in groups
            .into_iter()
            .flatten()
            .filter_map(Value::as_mapping_mut)
        {
            let Some(uses) = group.get_mut("use").and_then(Value::as_sequence_mut) else {
                continue;
            };
            uses.retain(|u| u.as_str().is_none_or(|u| !proxy_providers.contains(u)));
            if uses.is_empty() {
                group.remove("use");
                let members = group.get("proxies").and_then(Value::as_sequence);
                if members.is_none_or(|m| m.is_empty()) && !group.contains_key("include-all") {
                    set(group, "proxies", Value::Sequence(vec!["DIRECT".into()]));
                }
            }
        }
    }
    if !rule_providers.is_empty() {
        warnings.push(format!(
            "dropped local rule-providers and their rules: {}",
            sorted(&rule_providers)
        ));
        let uses_dropped = |rule: &Value| {
            let rule = rule.as_str().unwrap_or_default();
            rule_providers.iter().any(|name| {
                let needle = format!("RULE-SET,{name}");
                rule.match_indices(&needle).any(|(i, _)| {
                    matches!(
                        rule.as_bytes().get(i + needle.len()),
                        None | Some(b',' | b')')
                    )
                })
            })
        };
        if let Some(rules) = map.get_mut("rules").and_then(Value::as_sequence_mut) {
            rules.retain(|r| !uses_dropped(r));
        }
        let sub_rules = map.get_mut("sub-rules").and_then(Value::as_mapping_mut);
        for (_, rules) in sub_rules.into_iter().flatten() {
            if let Some(rules) = rules.as_sequence_mut() {
                rules.retain(|r| !uses_dropped(r));
            }
        }
    }
}

fn sorted(names: &HashSet<String>) -> String {
    let mut names: Vec<&str> = names.iter().map(String::as_str).collect();
    names.sort_unstable();
    names.join(", ")
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

fn group_names(map: &Mapping) -> Vec<String> {
    map.get("proxy-groups")
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

/// Drops unsupported/reserved proxies and `[filter]`ed nodes, then fixes every
/// reference to them (groups, rules, sub-rules, dialer-proxy).
fn sanitize_nodes(map: &mut Mapping, config: &Config, warnings: &mut Vec<String>) {
    let include = PatternSet::new(&config.filter.include);
    let exclude = PatternSet::new(&config.filter.exclude);
    let mut removed = HashSet::new();
    let mut unsafe_types = Vec::new();
    let mut filtered = 0usize;
    if let Some(proxies) = map.get_mut("proxies").and_then(Value::as_sequence_mut) {
        proxies.retain(|p| {
            let Some(name) = name_of(p) else { return false };
            let kind = p.get("type").and_then(Value::as_str).unwrap_or_default();
            let drop = if !PROXY_TYPES.contains(&kind.to_ascii_lowercase().as_str()) {
                unsafe_types.push(format!("{name} ({kind})"));
                true
            } else if BUILTIN_NAMES.contains(&name.as_str()) {
                unsafe_types.push(format!("{name} (reserved name)"));
                true
            } else if !keep(&name, &include, &exclude) {
                filtered += 1;
                true
            } else {
                false
            };
            if drop {
                removed.insert(name);
            }
            !drop
        });
    }
    if !unsafe_types.is_empty() {
        warnings.push(format!(
            "dropped unsupported proxies: {}",
            unsafe_types.join(", ")
        ));
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
            let existing = provider
                .get("exclude-type")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let excluded = match existing {
                Some(e) if !e.is_empty() => format!("{e}|{PROVIDER_EXCLUDED_TYPES}"),
                _ => PROVIDER_EXCLUDED_TYPES.to_owned(),
            };
            set(provider, "exclude-type", excluded);
            if let Some(payload) = provider.get_mut("payload").and_then(Value::as_sequence_mut) {
                payload.retain(|p| {
                    let kind = p.get("type").and_then(Value::as_str).unwrap_or_default();
                    PROXY_TYPES.contains(&kind.to_ascii_lowercase().as_str())
                });
            }
            for (key, patterns) in [("filter", &include), ("exclude-filter", &exclude)] {
                if patterns.is_empty() {
                    continue;
                }
                if provider.contains_key(key) {
                    warnings.push(format!(
                        "provider {} already has a {key}; replaced by [filter]",
                        name.as_str().unwrap_or("?")
                    ));
                }
                set(provider, key, patterns.to_regex());
            }
        }
    }
    if filtered > 0 {
        crate::debug!("[filter] dropped {filtered} node(s)");
    }
    if !removed.is_empty() {
        drop_references(map, &removed, warnings);
    }
    if (!include.is_empty() || !exclude.is_empty())
        && inline_names(map).is_empty()
        && !has_providers
    {
        warnings.push("[filter] removed every node of the subscription".into());
    }
}

/// Rewrites references to removed nodes so the config stays valid.
fn drop_references(map: &mut Mapping, removed: &HashSet<String>, warnings: &mut Vec<String>) {
    if let Some(groups) = map.get_mut("proxy-groups").and_then(Value::as_sequence_mut) {
        for group in groups.iter_mut().filter_map(Value::as_mapping_mut) {
            let uses_providers = group.contains_key("use") || group.contains_key("include-all");
            let Some(members) = group.get_mut("proxies").and_then(Value::as_sequence_mut) else {
                continue;
            };
            members.retain(|m| m.as_str().is_none_or(|n| !removed.contains(n)));
            if members.is_empty() && !uses_providers {
                members.push(Value::from("DIRECT"));
                let name = group.get("name").and_then(Value::as_str).unwrap_or("?");
                warnings.push(format!(
                    "group {name:?} lost all its nodes; it now points to DIRECT"
                ));
            }
        }
    }
    let mut rewritten = 0usize;
    let mut fix_rules = |rules: &mut Vec<Value>| {
        for rule in rules.iter_mut() {
            if let Some(fixed) = rule.as_str().and_then(|r| retarget_rule(r, removed)) {
                *rule = Value::from(fixed);
                rewritten += 1;
            }
        }
    };
    if let Some(rules) = map.get_mut("rules").and_then(Value::as_sequence_mut) {
        fix_rules(rules);
    }
    if let Some(sub_rules) = map.get_mut("sub-rules").and_then(Value::as_mapping_mut) {
        for (_, rules) in sub_rules.iter_mut() {
            if let Some(rules) = rules.as_sequence_mut() {
                fix_rules(rules);
            }
        }
    }
    if rewritten > 0 {
        warnings.push(format!(
            "{rewritten} rule(s) targeted removed nodes and now go DIRECT"
        ));
    }
    let dialer_of = |m: &Mapping| {
        m.get("dialer-proxy")
            .and_then(Value::as_str)
            .is_some_and(|d| removed.contains(d))
    };
    if let Some(proxies) = map.get_mut("proxies").and_then(Value::as_sequence_mut) {
        for proxy in proxies.iter_mut().filter_map(Value::as_mapping_mut) {
            if dialer_of(proxy) {
                proxy.remove("dialer-proxy");
            }
        }
    }
    if let Some(providers) = map
        .get_mut("proxy-providers")
        .and_then(Value::as_mapping_mut)
    {
        for (_, provider) in providers.iter_mut() {
            if let Some(over) = provider.get_mut("override").and_then(Value::as_mapping_mut)
                && dialer_of(over)
            {
                over.remove("dialer-proxy");
            }
        }
    }
}

/// `TYPE,payload,TARGET[,opts]` / `MATCH,TARGET` / logical rules with
/// parenthesised payloads: returns the rule with a removed target sent DIRECT.
fn retarget_rule(rule: &str, removed: &HashSet<String>) -> Option<String> {
    let mut parts = Vec::new();
    let (mut depth, mut start) = (0i32, 0usize);
    for (i, ch) in rule.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&rule[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&rule[start..]);
    let index = if parts.first()?.trim().eq_ignore_ascii_case("MATCH") {
        1
    } else {
        2
    };
    let target = parts.get(index)?.trim();
    if !removed.contains(target) {
        return None;
    }
    let mut fixed: Vec<&str> = parts.clone();
    fixed[index] = "DIRECT";
    Some(fixed.join(","))
}

/// A name not used by any proxy, group or built-in.
fn free_name(base: &str, taken: &HashSet<String>) -> String {
    let mut name = base.to_owned();
    let mut n = 2;
    while taken.contains(&name) || BUILTIN_NAMES.contains(&name.as_str()) {
        name = format!("{base}-{n}");
        n += 1;
    }
    name
}

/// Gives configs without groups (link lists, Xray JSON, bare `proxies:`) a
/// selector, a url-test group and LAN-direct rules.
fn ensure_groups(map: &mut Mapping) {
    let has_groups = map
        .get("proxy-groups")
        .and_then(Value::as_sequence)
        .is_some_and(|g| !g.is_empty());
    if has_groups {
        return;
    }
    let names: Vec<Value> = inline_names(map).into_iter().map(Value::from).collect();
    let providers = provider_names(map);
    let taken: HashSet<String> = inline_names(map).into_iter().collect();
    let proxy = free_name("PROXY", &taken);
    let auto = free_name("AUTO", &taken);
    let group = |name: &str, kind: &str, mut proxies: Vec<Value>| {
        let mut g = Mapping::new();
        set(&mut g, "name", name);
        set(&mut g, "type", kind);
        if !providers.is_empty() {
            set(&mut g, "use", Value::Sequence(providers.clone()));
        } else if proxies.is_empty() {
            proxies.push(Value::from("DIRECT"));
        }
        set(&mut g, "proxies", Value::Sequence(proxies));
        if kind == "url-test" {
            set(&mut g, "url", HEALTH_CHECK_URL);
            set(&mut g, "interval", 600);
            set(&mut g, "tolerance", 50);
            set(&mut g, "lazy", true);
        }
        Value::Mapping(g)
    };
    let mut select = vec![Value::from(auto.as_str())];
    select.extend(names.iter().cloned());
    select.push(Value::from("DIRECT"));
    let groups = vec![
        group(&proxy, "select", select),
        group(&auto, "url-test", names),
    ];
    set(map, "proxy-groups", Value::Sequence(groups));
    // Provider rules would reference groups that do not exist; ours route via PROXY.
    let mut rules: Vec<Value> = LAN_RULES.iter().map(|r| Value::from(*r)).collect();
    rules.push(Value::from(format!("MATCH,{proxy}")));
    set(map, "rules", Value::Sequence(rules));
}

/// `[[groups]]`: user-defined auto-switching groups, listed first.
fn apply_custom_groups(
    map: &mut Mapping,
    config: &Config,
    warnings: &mut Vec<String>,
) -> Result<()> {
    if config.groups.is_empty() {
        return Ok(());
    }
    let names = inline_names(map);
    let existing_groups = group_names(map);
    for g in &config.groups {
        if names.contains(&g.name) || existing_groups.contains(&g.name) {
            bail!(
                "[[groups]] name {:?} collides with a proxy or group of the subscription; rename it",
                g.name
            );
        }
    }
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
        bail!("proxy-groups is not a list");
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
    Ok(())
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
    // FlClashX writes this unconditionally (lib/state.dart), replacing the provider's.
    if config.subscription.client == ClientKind::FlClashX {
        let core_version = config
            .subscription
            .core_version
            .as_deref()
            .unwrap_or(crate::client::emulation::FLCLASHX_CORE_VERSION);
        set(map, "global-ua", format!("clash.meta/{core_version}"));
    }
}

fn apply_gateway(map: &mut Mapping, config: &Config, params: &Params<'_>) {
    let gw = &config.gateway;
    let tun = child(map, "tun");
    set(tun, "enable", true);
    set(tun, "device", crate::gateway::killswitch::TUN_DEVICE);
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
    // Panel hosts must resolve to real addresses. How depends on the filter mode:
    // blacklist (default) lists real-IP domains, whitelist lists fake-IP domains
    // (nothing to do), rule mode takes `DOMAIN,host,real-ip` rules, first match wins.
    let filter_mode = dns
        .get("fake-ip-filter-mode")
        .and_then(Value::as_str)
        .unwrap_or("blacklist")
        .to_owned();
    if !params.panel_hosts.is_empty() && filter_mode != "whitelist" {
        let filter = dns
            .entry(Value::from("fake-ip-filter"))
            .or_insert_with(|| Value::Sequence(Vec::new()));
        if let Some(filter) = filter.as_sequence_mut() {
            if filter_mode == "rule" {
                let rules = params
                    .panel_hosts
                    .iter()
                    .map(|h| Value::from(format!("DOMAIN,{h},real-ip")));
                filter.splice(0..0, rules);
            } else {
                filter.extend(params.panel_hosts.iter().map(|h| Value::from(h.as_str())));
            }
        }
    }
    if !params.panel_ips.is_empty() {
        let routes = params
            .panel_ips
            .iter()
            .map(|ip| match ip {
                IpAddr::V4(v4) => Value::from(format!("{v4}/32")),
                IpAddr::V6(v6) => Value::from(format!("{v6}/128")),
            })
            .collect();
        let tun = child(map, "tun");
        set(tun, "route-exclude-address", Value::Sequence(routes));
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

    fn params(secret: &'static str) -> Params<'static> {
        Params {
            secret,
            mode: "rule",
            panel_hosts: &[],
            panel_ips: &[],
        }
    }

    fn parsed(yaml: &str) -> Content {
        body::parse(yaml.as_bytes(), None).unwrap()
    }

    fn built(content: &Content, config: &Config) -> Value {
        serde_norway::from_str(
            &build(content, config, &params("s3cret"))
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
    fn providers_cannot_choose_local_paths() {
        let yaml = r#"
proxies:
  - {name: NL, type: ss, server: nl.example.com, port: 1, cipher: aes-128-gcm, password: p}
proxy-providers:
  remote: {type: http, url: "https://p.example/sub", path: ./config.yaml}
  local: {type: file, path: /etc/passwd}
  inl: {type: inline, payload: []}
rule-providers:
  steal: {type: http, behavior: domain, url: "https://p.example/r", path: ./cache.db}
  local-rules: {type: file, behavior: domain, path: ./secret}
proxy-groups:
  - {name: G, type: select, use: [local]}
  - {name: H, type: select, use: [remote, local], proxies: [NL]}
rules:
  - RULE-SET,steal,G
  - RULE-SET,local-rules,G
  - AND,((RULE-SET,local-rules),(NETWORK,tcp)),H
  - RULE-SET,local-rules-2,H
  - MATCH,H
"#;
        let content = crate::subscription::body::parse(yaml.as_bytes(), None).unwrap();
        let built = build(&content, &Config::default(), &params("s")).unwrap();
        let v: Value = serde_norway::from_str(&built.config_yaml).unwrap();
        let pp = &v["proxy-providers"];
        assert!(pp["remote"].get("path").is_none());
        assert!(pp.get("local").is_none());
        assert!(pp.get("inl").is_some());
        assert!(v["rule-providers"]["steal"].get("path").is_none());
        assert!(v["rule-providers"].get("local-rules").is_none());
        let groups = v["proxy-groups"].as_sequence().unwrap();
        assert_eq!(groups[0]["proxies"][0].as_str(), Some("DIRECT"));
        assert!(groups[0].get("use").is_none());
        assert_eq!(groups[1]["use"][0].as_str(), Some("remote"));
        let rules: Vec<&str> = v["rules"]
            .as_sequence()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(rules.contains(&"RULE-SET,steal,G"));
        assert!(
            rules.contains(&"RULE-SET,local-rules-2,H"),
            "prefix names are distinct"
        );
        assert!(!rules.iter().any(|r| r.contains("RULE-SET,local-rules,")
            || r.contains("RULE-SET,local-rules)")));
    }

    #[test]
    fn sanitize_nodes_drops_unsupported_and_reserved() {
        let yaml = r#"
proxies:
  - {name: NL, type: ss, server: nl.example, port: 1, cipher: aes-128-gcm, password: p}
  - {name: Mesh, type: tailscale, server: mesh.example, port: 1}
  - {name: DIRECT, type: vless, server: bad.example, port: 443, uuid: x}
proxy-groups:
  - {name: G, type: select, proxies: [NL, Mesh, DIRECT]}
rules:
  - MATCH,G
"#;
        let built = build(&parsed(yaml), &Config::default(), &params("s")).unwrap();
        let v: Value = serde_norway::from_str(&built.config_yaml).unwrap();
        assert_eq!(names(&v["proxies"]), ["NL"]);
        assert_eq!(names(&v["proxy-groups"][0]["proxies"]), ["NL"]);
        let warning = built
            .warnings
            .iter()
            .find(|w| w.contains("dropped unsupported proxies"))
            .expect("a warning about dropped proxies");
        assert!(warning.contains("Mesh (tailscale)"));
        assert!(warning.contains("DIRECT (reserved name)"));
    }

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
    fn kill_switch_marks_mihomo_and_pins_the_tun_name() {
        let mut config: Config =
            toml::from_str("[mihomo]\nrouting-mark = 1\ntun = { device = \"utun9\" }").unwrap();
        config.gateway.enable = true;
        let v = built(&parsed(REMNAWAVE), &config);
        assert_eq!(
            v["tun"]["device"].as_str(),
            Some("utun9"),
            "user wins without kill switch"
        );
        assert_eq!(v["routing-mark"].as_u64(), Some(1));
        config.gateway.kill_switch = true;
        let v = built(&parsed(REMNAWAVE), &config);
        assert_eq!(
            v["tun"]["device"].as_str(),
            Some(crate::gateway::killswitch::TUN_DEVICE)
        );
        assert_eq!(
            v["routing-mark"].as_u64(),
            Some(u64::from(crate::gateway::killswitch::MARK))
        );
    }

    #[test]
    fn gateway_keeps_the_panel_off_the_tunnel() {
        let mut config = Config::default();
        config.gateway.enable = true;
        let hosts = ["sub.example.com".to_owned()];
        let ips: [IpAddr; 2] = [
            "203.0.113.7".parse().unwrap(),
            "2001:db8::7".parse().unwrap(),
        ];
        let params = Params {
            panel_hosts: &hosts,
            panel_ips: &ips,
            ..params("s")
        };
        let render = |yaml: &str| -> Value {
            let built = build(&parsed(yaml), &config, &params).unwrap();
            serde_norway::from_str(&built.config_yaml).unwrap()
        };
        let base = "proxies: [{name: a, type: ss, server: h, port: 1080}]\n";
        let v = render(base);
        assert_eq!(
            v["dns"]["fake-ip-filter"][0].as_str(),
            Some("sub.example.com")
        );
        let routes: Vec<&str> = v["tun"]["route-exclude-address"]
            .as_sequence()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(routes, ["203.0.113.7/32", "2001:db8::7/128"]);

        let v = render(&format!(
            "{base}dns: {{fake-ip-filter-mode: rule, fake-ip-filter: ['MATCH,fake-ip']}}\n"
        ));
        assert_eq!(
            v["dns"]["fake-ip-filter"][0].as_str(),
            Some("DOMAIN,sub.example.com,real-ip")
        );
        let v = render(&format!(
            "{base}dns: {{fake-ip-filter-mode: whitelist, fake-ip-filter: [x.example]}}\n"
        ));
        assert_eq!(v["dns"]["fake-ip-filter"].as_sequence().unwrap().len(), 1);
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
        let built = build(&content, &Config::default(), &params("x")).unwrap();
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
        assert_eq!(
            v["rules"].as_sequence().unwrap().last().unwrap().as_str(),
            Some("MATCH,→ Remnawave")
        );
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
        let built = build(&parsed(THREE_NODES), &config, &params("s")).unwrap();
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
            &build(&parsed(yaml), config, &params("s"))
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
            serde_norway::from_str(&build(&content, &config, &params("s")).unwrap().config_yaml)
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
            serde_norway::from_str(&build(&content, &config, &params("s")).unwrap().config_yaml)
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
