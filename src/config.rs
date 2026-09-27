//! Application configuration.
//!
//! Precedence (lowest to highest): built-in defaults → TOML file → `MIHOMYAK_*`
//! environment variables. Docker deployments usually need nothing but env vars.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::emulation::ClientKind;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub subscription: Subscription,
    pub update: Update,
    pub filter: Filter,
    /// Custom proxy groups (auto-switching between chosen nodes).
    pub groups: Vec<Group>,
    pub rules: Rules,
    pub device: Device,
    pub core: Core,
    pub gateway: Gateway,
    /// Raw mihomo keys deep-merged over the generated config (highest priority).
    pub mihomo: toml::Table,

    #[serde(skip)]
    pub data_dir: PathBuf,
    #[serde(skip)]
    pub source: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Subscription {
    pub url: Option<String>,
    /// Which real client to impersonate.
    pub client: ClientKind,
    /// Overrides the emulated app version (e.g. `0.4.3`) without a rebuild.
    pub app_version: Option<String>,
    /// Overrides Happ's build id (`Happ/<ver>/Linux/<build>…`).
    pub app_build: Option<String>,
    /// Overrides the emulated mihomo core version shown in FlClashX's User-Agent.
    pub core_version: Option<String>,
    /// Replaces the User-Agent value (keeps the emulated header order).
    pub user_agent: Option<String>,
    /// Extra `Name: value` headers; a name that already exists is replaced in place.
    pub headers: Vec<String>,
    /// Fetch through an HTTP proxy (`http://host:port`, tunnelled with CONNECT).
    pub proxy: Option<String>,
    /// Apply configs even when they look like a provider stub.
    pub accept_stub: bool,
}

impl Default for Subscription {
    fn default() -> Self {
        Self {
            url: None,
            client: ClientKind::FlClashX,
            app_version: None,
            app_build: None,
            core_version: None,
            user_agent: None,
            headers: Vec::new(),
            proxy: None,
            accept_stub: false,
        }
    }
}

/// When the subscription is refreshed. All triggers combine: the earliest wins.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Update {
    /// `auto` (provider's `profile-update-interval`, else 24h), `off`, or e.g. `12h`.
    pub interval: Interval,
    /// Cron expressions in local time (`TZ`), e.g. `["0 5 * * *"]`.
    pub cron: Vec<String>,
    /// Refetch at every start instead of reusing a cache that is not yet due.
    pub on_start: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Interval {
    #[default]
    Auto,
    Off,
    Fixed(Duration),
}

/// Node whitelist/blacklist by name (globs: `*`, `?`; case-insensitive).
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Filter {
    /// Keep only nodes matching one of these (empty: keep all).
    pub include: Vec<String>,
    /// Drop nodes matching one of these.
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GroupType {
    /// First alive node in pattern order.
    Fallback,
    /// Lowest latency node.
    UrlTest,
    /// Spread connections over nodes.
    LoadBalance,
    /// Manual choice.
    Select,
}

impl GroupType {
    pub fn as_mihomo(self) -> &'static str {
        match self {
            GroupType::Fallback => "fallback",
            GroupType::UrlTest => "url-test",
            GroupType::LoadBalance => "load-balance",
            GroupType::Select => "select",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    pub name: String,
    #[serde(rename = "type", default = "default_group_type")]
    pub kind: GroupType,
    /// Name patterns in priority order (empty: every node).
    #[serde(default)]
    pub nodes: Vec<String>,
    #[serde(default = "default_health_url")]
    pub url: String,
    #[serde(default = "default_health_interval")]
    pub interval: HumanDuration,
    /// url-test: switch only when the new node is this many ms faster.
    #[serde(default = "default_tolerance")]
    pub tolerance: u32,
    /// Offer this group first in the subscription's selector groups, so it is
    /// the default choice.
    #[serde(default)]
    pub default: bool,
}

fn default_group_type() -> GroupType {
    GroupType::Fallback
}

fn default_health_url() -> String {
    crate::profile::HEALTH_CHECK_URL.into()
}

fn default_health_interval() -> HumanDuration {
    HumanDuration(Duration::from_secs(180))
}

fn default_tolerance() -> u32 {
    50
}

/// A duration written as `90s`, `3m`, `1h`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HumanDuration(pub Duration);

impl<'de> Deserialize<'de> for HumanDuration {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(de)?;
        crate::util::parse_duration(&raw)
            .map(HumanDuration)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Rules {
    /// mihomo rules placed before the subscription's own, e.g.
    /// `"DOMAIN-SUFFIX,lan,DIRECT"` or `"DOMAIN-SUFFIX,example.com,Auto"`.
    pub prepend: Vec<String>,
    /// Built-in rule sets inserted after `prepend`: `ru-direct`.
    pub presets: Vec<Preset>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Preset {
    /// Russian sites and IPs bypass the proxy.
    RuDirect,
}

impl std::str::FromStr for Preset {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim() {
            "ru-direct" => Ok(Self::RuDirect),
            other => bail!("unknown rules preset {other:?} (known: ru-direct)"),
        }
    }
}

impl std::fmt::Display for Interval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => f.write_str("auto"),
            Self::Off => f.write_str("off"),
            Self::Fixed(d) => f.write_str(&crate::util::fmt_duration(*d)),
        }
    }
}

impl<'de> Deserialize<'de> for Interval {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(de)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

impl std::str::FromStr for Interval {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        if s.eq_ignore_ascii_case("auto") {
            return Ok(Self::Auto);
        }
        if s.eq_ignore_ascii_case("off") {
            return Ok(Self::Off);
        }
        let d = crate::util::parse_duration(s)?;
        if d < crate::updater::MIN_INTERVAL {
            bail!("update interval {s:?} is shorter than the 5 minute minimum");
        }
        Ok(Self::Fixed(d))
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Device {
    /// Seed every client derives its HWID from (like `/etc/machine-id`).
    /// Default: generated once and stored in the data directory.
    pub machine_id: Option<String>,
    /// Final `x-hwid` value, bypassing the client-specific derivation.
    pub hwid: Option<String>,
    /// Send the `x-hwid`/`x-device-*` headers (FlClashX "send device headers" toggle).
    pub send_headers: bool,
    /// os-release file describing the emulated desktop.
    pub os_release: PathBuf,
    /// Overrides for individual os-release fields.
    pub os_name: Option<String>,
    pub os_version: Option<String>,
    pub os_pretty_name: Option<String>,
    /// Hostname reported by Happ (`X-Device-Model: <hostname>_<arch>`).
    /// Default: the kernel hostname (in Docker set `hostname:` in compose).
    pub hostname: Option<String>,
    /// UI locale reported by Happ: `en` (default) or e.g. `ru_RU`.
    pub locale: String,
}

impl Default for Device {
    fn default() -> Self {
        Self {
            machine_id: None,
            hwid: None,
            send_headers: true,
            os_release: PathBuf::from("/etc/os-release"),
            os_name: None,
            os_version: None,
            os_pretty_name: None,
            hostname: None,
            locale: "en".into(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Core {
    /// mihomo executable (name looked up in PATH, or a path).
    pub bin: PathBuf,
    /// `host:port` or `unix:/path/to/socket` for mihomo's REST API.
    pub controller: String,
    /// API secret. Default: generated once and stored in the data directory.
    pub secret: Option<String>,
    /// HTTP+SOCKS5 proxy port (0 disables it, e.g. for TUN-only gateways).
    pub mixed_port: u16,
    /// Accept proxy connections from other hosts/containers. Off by default:
    /// the port then only listens on loopback.
    pub allow_lan: bool,
    /// With `allow_lan`, only these source networks may connect. Defaults to
    /// private ranges so a VPS with a public IP never becomes an open proxy.
    pub lan_allowed_ips: Vec<String>,
    /// Proxy credentials `user:password`; loopback clients are exempt.
    pub auth: Vec<String>,
    pub bind_address: String,
    pub log_level: String,
    /// Default routing mode (`rule`, `global`, `direct`); `mihomyak mode` persists changes.
    pub mode: String,
    /// Go runtime soft memory limit for mihomo (`GOMEMLIMIT`, e.g. `64MiB`).
    pub memory_limit: Option<String>,
    /// Directory with bundled geodata (geoip.metadb, geosite.dat, …) copied into
    /// mihomo's home on start when missing, so GEOIP/GEOSITE rules work offline.
    pub geodata_dir: Option<PathBuf>,
}

impl Default for Core {
    fn default() -> Self {
        Self {
            bin: PathBuf::from("mihomo"),
            controller: "127.0.0.1:9090".into(),
            secret: None,
            mixed_port: 7890,
            allow_lan: false,
            lan_allowed_ips: [
                "10.0.0.0/8",
                "172.16.0.0/12",
                "192.168.0.0/16",
                "100.64.0.0/10",
                "127.0.0.0/8",
                "fc00::/7",
                "fe80::/10",
                "::1/128",
            ]
            .map(String::from)
            .to_vec(),
            auth: Vec::new(),
            bind_address: "*".into(),
            log_level: "warning".into(),
            mode: "rule".into(),
            memory_limit: None,
            geodata_dir: None,
        }
    }
}

/// Transparent gateway mode: TUN with auto-route, so every packet routed through
/// this network namespace (other containers, LAN clients) goes through mihomo.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Gateway {
    pub enable: bool,
    /// TUN stack: `system` (lightest), `gvisor` or `mixed`.
    pub stack: String,
    /// nftables-based redirect for TCP (faster, needs nf_tables in the kernel).
    pub auto_redirect: bool,
    /// Where mihomo's DNS server listens. TUN hijacks port 53 regardless; expose
    /// it (e.g. `0.0.0.0:1053`) only if LAN clients should query it directly.
    pub dns_listen: String,
    /// Block traffic that would bypass mihomo while it is down (see `killswitch`).
    pub kill_switch: bool,
}

impl Default for Gateway {
    fn default() -> Self {
        Self {
            enable: false,
            stack: "system".into(),
            auto_redirect: false,
            dns_listen: "127.0.0.1:1053".into(),
            kill_switch: false,
        }
    }
}

impl Config {
    /// Loads the TOML file (if any) and applies environment overrides.
    pub fn load(explicit_path: Option<&Path>, data_dir: Option<&Path>) -> Result<Self> {
        let env_path = std::env::var_os("MIHOMYAK_CONFIG").map(PathBuf::from);
        let path = explicit_path.map(Path::to_path_buf).or(env_path);
        let mut config = match &path {
            Some(p) if p.exists() => Self::from_file(p)?,
            Some(p) if explicit_path.is_some() => bail!("config file {} not found", p.display()),
            _ => match default_config_path().filter(|p| p.exists()) {
                Some(p) => Self::from_file(&p)?,
                None => Self::default(),
            },
        };
        config.apply_env(&|key| std::env::var(key).ok())?;
        config.data_dir = match data_dir {
            Some(dir) => dir.to_path_buf(),
            None => std::env::var_os("MIHOMYAK_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(default_data_dir),
        };
        config.validate()?;
        Ok(config)
    }

    fn from_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read config {}", path.display()))?;
        let mut config: Self =
            toml::from_str(&text).with_context(|| format!("parse config {}", path.display()))?;
        if config.holds_secrets() {
            warn_if_exposed(path);
        }
        config.source = Some(path.to_path_buf());
        Ok(config)
    }

    /// Whether the file itself holds credentials or the device identity (the
    /// URL often comes from the environment instead).
    fn holds_secrets(&self) -> bool {
        self.subscription.url.is_some()
            || self.core.secret.is_some()
            || !self.core.auth.is_empty()
            || self.device.machine_id.is_some()
            || self.device.hwid.is_some()
    }

    /// `env` is injected for testability.
    fn apply_env(&mut self, env: &dyn Fn(&str) -> Option<String>) -> Result<()> {
        let flag = |key: &str| -> Result<Option<bool>> {
            env(key)
                .map(|v| match v.to_ascii_lowercase().as_str() {
                    "1" | "true" | "yes" | "on" => Ok(true),
                    "0" | "false" | "no" | "off" | "" => Ok(false),
                    _ => bail!("{key}: expected a boolean, got {v:?}"),
                })
                .transpose()
        };
        let sub = &mut self.subscription;
        if let Some(v) = env("MIHOMYAK_SUB_URL") {
            sub.url = Some(v);
        }
        if let Some(v) = env("MIHOMYAK_CLIENT") {
            sub.client = v.parse()?;
        }
        if let Some(v) = env("MIHOMYAK_APP_VERSION") {
            sub.app_version = Some(v);
        }
        if let Some(v) = env("MIHOMYAK_USER_AGENT") {
            sub.user_agent = Some(v);
        }

        if let Some(v) = env("MIHOMYAK_FETCH_PROXY") {
            sub.proxy = Some(v).filter(|s| !s.is_empty());
        }
        if let Some(v) = flag("MIHOMYAK_ACCEPT_STUB")? {
            sub.accept_stub = v;
        }
        let update = &mut self.update;
        if let Some(v) = env("MIHOMYAK_UPDATE_INTERVAL") {
            update.interval = v.parse().context("MIHOMYAK_UPDATE_INTERVAL")?;
        }
        if let Some(v) = env("MIHOMYAK_UPDATE_CRON") {
            update.cron = split_list(&v);
        }
        if let Some(v) = flag("MIHOMYAK_UPDATE_ON_START")? {
            update.on_start = v;
        }
        if let Some(v) = env("MIHOMYAK_RULES_PRESETS") {
            self.rules.presets = split_list(&v)
                .iter()
                .map(|p| p.parse())
                .collect::<Result<_>>()?;
        }
        if let Some(v) = env("MIHOMYAK_INCLUDE") {
            self.filter.include = split_list(&v);
        }
        if let Some(v) = env("MIHOMYAK_EXCLUDE") {
            self.filter.exclude = split_list(&v);
        }
        let dev = &mut self.device;
        if let Some(v) = env("MIHOMYAK_MACHINE_ID") {
            dev.machine_id = Some(v);
        }
        if let Some(v) = env("MIHOMYAK_HWID") {
            dev.hwid = Some(v);
        }
        if let Some(v) = env("MIHOMYAK_HOSTNAME") {
            dev.hostname = Some(v);
        }
        if let Some(v) = env("MIHOMYAK_LOCALE") {
            dev.locale = v;
        }
        if let Some(v) = env("MIHOMYAK_OS_RELEASE") {
            dev.os_release = PathBuf::from(v);
        }
        let core = &mut self.core;
        if let Some(v) = env("MIHOMYAK_CORE_BIN") {
            core.bin = PathBuf::from(v);
        }
        if let Some(v) = env("MIHOMYAK_CONTROLLER") {
            core.controller = v;
        }
        if let Some(v) = env("MIHOMYAK_SECRET") {
            core.secret = Some(v);
        }
        if let Some(v) = flag("MIHOMYAK_ALLOW_LAN")? {
            core.allow_lan = v;
        }
        if let Some(v) = env("MIHOMYAK_PROXY_AUTH") {
            core.auth = split_list(&v);
        }
        if let Some(v) = env("MIHOMYAK_MIXED_PORT") {
            core.mixed_port = v.parse().context("MIHOMYAK_MIXED_PORT")?;
        }
        if let Some(v) = env("MIHOMYAK_MODE") {
            core.mode = v;
        }
        if let Some(v) = env("MIHOMYAK_LOG_LEVEL") {
            core.log_level = v;
        }
        if let Some(v) = env("MIHOMYAK_GEODATA_DIR") {
            core.geodata_dir = Some(PathBuf::from(v)).filter(|p| !p.as_os_str().is_empty());
        }
        if let Some(v) = env("MIHOMYAK_MEMORY_LIMIT") {
            core.memory_limit = Some(v).filter(|s| !s.is_empty());
        }
        if let Some(v) = flag("MIHOMYAK_GATEWAY")? {
            self.gateway.enable = v;
        }
        if let Some(v) = flag("MIHOMYAK_KILL_SWITCH")? {
            self.gateway.kill_switch = v;
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if let Some(url) = &self.subscription.url {
            crate::http::Url::parse(url).context("subscription.url")?;
        }
        if let Some(proxy) = &self.subscription.proxy {
            let url = crate::http::Url::parse(proxy).context("subscription.proxy")?;
            if url.scheme != crate::http::Scheme::Http {
                bail!("subscription.proxy must be an http:// proxy");
            }
        }
        for header in &self.subscription.headers {
            let Some((name, _)) = header.split_once(':') else {
                bail!("subscription.headers entry {header:?} must look like \"Name: value\"");
            };
            let token = |b: u8| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b);
            if name.trim().is_empty() || !name.trim().bytes().all(token) {
                bail!("subscription.headers entry {header:?} has an invalid header name");
            }
        }
        // These end up in request headers: CR/LF would inject extra headers.
        let header_values = [
            (
                "subscription.headers",
                self.subscription.headers.iter().collect::<Vec<_>>(),
            ),
            (
                "subscription.user_agent",
                self.subscription.user_agent.iter().collect(),
            ),
            (
                "subscription.app_version",
                self.subscription.app_version.iter().collect(),
            ),
            (
                "subscription.app_build",
                self.subscription.app_build.iter().collect(),
            ),
            (
                "subscription.core_version",
                self.subscription.core_version.iter().collect(),
            ),
            ("device.hwid", self.device.hwid.iter().collect()),
            ("device.machine_id", self.device.machine_id.iter().collect()),
            ("device.hostname", self.device.hostname.iter().collect()),
            ("device.os_name", self.device.os_name.iter().collect()),
            ("device.os_version", self.device.os_version.iter().collect()),
            (
                "device.os_pretty_name",
                self.device.os_pretty_name.iter().collect(),
            ),
            ("device.locale", vec![&self.device.locale]),
        ];
        for (key, values) in header_values {
            if values.iter().any(|v| v.chars().any(char::is_control)) {
                bail!("{key} must not contain control characters (line breaks, tabs, …)");
            }
        }
        for cron in &self.update.cron {
            cron.parse::<crate::schedule::Cron>()?;
        }
        let mut names = std::collections::HashSet::new();
        for group in &self.groups {
            if group.name.trim().is_empty() || !names.insert(group.name.as_str()) {
                bail!(
                    "[[groups]] names must be non-empty and unique ({:?})",
                    group.name
                );
            }
            if ["DIRECT", "REJECT", "GLOBAL", "PROXY", "AUTO"].contains(&group.name.as_str()) {
                bail!("[[groups]] name {:?} is reserved", group.name);
            }
        }
        for cred in &self.core.auth {
            match cred.split_once(':') {
                Some((user, pass)) if !user.is_empty() && !pass.is_empty() => {}
                _ => bail!("core.auth entries must look like \"user:password\""),
            }
        }
        if let Some(host) = self.core.controller.rsplit_once(':').map(|(h, _)| h) {
            let loopback = matches!(
                host.trim_matches(['[', ']']),
                "127.0.0.1" | "localhost" | "::1"
            ) || host.starts_with("127.");
            if !self.core.controller.starts_with("unix:") && !loopback {
                if self.core.secret.as_deref() == Some("") {
                    bail!(
                        "core.controller {} is reachable from the network: an empty secret is not allowed",
                        self.core.controller
                    );
                }
                crate::warn!(
                    "mihomo API on {} is reachable from the network; keep the secret private",
                    self.core.controller
                );
            }
        }
        if !is_mode(&self.core.mode) {
            bail!("core.mode must be rule, global or direct");
        }
        if !["system", "gvisor", "mixed"].contains(&self.gateway.stack.as_str()) {
            bail!("gateway.stack must be system, gvisor or mixed");
        }
        if self.gateway.kill_switch && !self.gateway.enable {
            bail!("gateway.kill_switch needs gateway.enable (MIHOMYAK_GATEWAY=1)");
        }
        Ok(())
    }

    pub fn subscription_url(&self) -> Result<&str> {
        self.subscription.url.as_deref().filter(|u| !u.is_empty()).context(
            "no subscription URL: set MIHOMYAK_SUB_URL or [subscription] url in the config file",
        )
    }
}

/// Called for a config that holds the subscription URL or other secrets.
fn warn_if_exposed(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path)
        && meta.permissions().mode() & 0o077 != 0
    {
        crate::warn!(
            "{} is readable by other users and holds secrets such as the subscription URL (chmod 600)",
            path.display()
        );
    }
}

/// Env lists are `;`-separated (node names may contain commas).
fn split_list(value: &str) -> Vec<String> {
    value
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

pub fn is_mode(mode: &str) -> bool {
    matches!(mode, "rule" | "global" | "direct")
}

fn default_config_path() -> Option<PathBuf> {
    if is_root() {
        return Some(PathBuf::from("/etc/mihomyak/config.toml"));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("mihomyak/config.toml"))
}

fn default_data_dir() -> PathBuf {
    if is_root() {
        return PathBuf::from("/var/lib/mihomyak");
    }
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("mihomyak")
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn shipped_example_is_valid() {
        let config: Config = toml::from_str(include_str!("../examples/config.toml")).unwrap();
        config.validate().unwrap();
        assert_eq!(config.groups[0].name, "Auto");
    }

    #[test]
    fn parses_full_file() {
        let config: Config = toml::from_str(
            r#"
            [subscription]
            url = "https://sub.example.com/abc"
            client = "koala"
            headers = ["X-Extra: 1"]

            [update]
            interval = "6h"
            cron = ["0 5 * * *"]
            on_start = true

            [filter]
            exclude = ["*Россия*"]

            [[groups]]
            name = "Auto NL/DE"
            type = "url-test"
            nodes = ["*NL*", "*DE*"]
            interval = "5m"
            default = true

            [rules]
            prepend = ["DOMAIN-SUFFIX,lan,DIRECT"]

            [device]
            machine_id = "0d0af05ee8fd4dc29275718f2ce4dff1"

            [core]
            mixed_port = 7891
            memory_limit = "48MiB"

            [gateway]
            enable = true

            [mihomo]
            ipv6 = true
            dns = { enable = true }
            "#,
        )
        .unwrap();
        assert_eq!(config.subscription.client, ClientKind::Koala);
        assert_eq!(
            config.update.interval,
            Interval::Fixed(Duration::from_secs(6 * 3600))
        );
        assert!(config.update.on_start);
        assert_eq!(config.groups[0].kind, GroupType::UrlTest);
        assert_eq!(
            config.groups[0].interval,
            HumanDuration(Duration::from_secs(300))
        );
        assert_eq!(config.groups[0].tolerance, 50);
        assert_eq!(config.filter.exclude, ["*Россия*"]);
        assert_eq!(config.core.mixed_port, 7891);
        assert!(config.gateway.enable);
        assert_eq!(config.mihomo["ipv6"].as_bool(), Some(true));
        config.validate().unwrap();
    }

    #[test]
    fn rejects_unknown_keys_and_bad_values() {
        assert!(toml::from_str::<Config>("[subscription]\nurll = \"x\"").is_err());
        assert!(toml::from_str::<Config>("[update]\ninterval = \"10s\"").is_err());
        let dup: Config =
            toml::from_str("[[groups]]\nname = \"A\"\n[[groups]]\nname = \"A\"").unwrap();
        assert!(dup.validate().is_err());
        let bad_cron: Config = toml::from_str("[update]\ncron = [\"61 * * * *\"]").unwrap();
        assert!(bad_cron.validate().is_err());
        let lone_kill_switch: Config = toml::from_str("[gateway]\nkill_switch = true").unwrap();
        assert!(lone_kill_switch.validate().is_err());
        assert!(toml::from_str::<Config>("[subscription]\nclient = \"hiddify\"").is_err());
    }

    #[test]
    fn controller_off_loopback_needs_a_non_empty_secret() {
        let mut config = Config::default();
        config.core.controller = "0.0.0.0:9090".into();
        config.core.secret = Some(String::new());
        assert!(
            config.validate().is_err(),
            "an empty secret must not be exposed to the network"
        );

        config.core.secret = None;
        assert!(
            config.validate().is_ok(),
            "no configured secret is only warned about, not rejected"
        );

        config.core.secret = Some("s3cret".into());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn loopback_controller_needs_no_secret() {
        let mut config = Config::default();
        config.core.controller = "127.0.0.1:9090".into();
        config.core.secret = None;
        assert!(config.validate().is_ok());

        config.core.controller = "localhost:9090".into();
        assert!(config.validate().is_ok());

        config.core.controller = "unix:/run/mihomyak/mihomo.sock".into();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn only_files_with_secrets_need_private_permissions() {
        let holds = |text: &str| toml::from_str::<Config>(text).unwrap().holds_secrets();
        assert!(!holds(
            "[filter]\nexclude = [\"*RU*\"]\n[[groups]]\nname = \"A\""
        ));
        assert!(holds(
            "[subscription]\nurl = \"https://panel.example/sub/x\""
        ));
        assert!(holds("[core]\nauth = [\"user:pass\"]"));
        assert!(holds(
            "[device]\nmachine_id = \"0d0af05ee8fd4dc29275718f2ce4dff1\""
        ));
    }

    #[test]
    fn env_overrides_file() {
        let mut config = Config::default();
        let env: HashMap<&str, &str> = [
            ("MIHOMYAK_SUB_URL", "https://s.example/x"),
            ("MIHOMYAK_CLIENT", "koala"),
            ("MIHOMYAK_GATEWAY", "true"),
            ("MIHOMYAK_KILL_SWITCH", "1"),
            ("MIHOMYAK_UPDATE_INTERVAL", "auto"),
            ("MIHOMYAK_MIXED_PORT", "1080"),
            ("MIHOMYAK_UPDATE_CRON", "0 5 * * *; 30 17 * * *"),
            ("MIHOMYAK_EXCLUDE", "*test*;*Россия, Москва*"),
        ]
        .into();
        config
            .apply_env(&|k| env.get(k).map(|v| v.to_string()))
            .unwrap();
        assert_eq!(
            config.subscription.url.as_deref(),
            Some("https://s.example/x")
        );
        assert_eq!(config.subscription.client, ClientKind::Koala);
        assert!(config.gateway.enable && config.gateway.kill_switch);
        config.validate().unwrap();
        assert_eq!(config.core.mixed_port, 1080);
        assert_eq!(config.update.cron, ["0 5 * * *", "30 17 * * *"]);
        assert_eq!(config.filter.exclude, ["*test*", "*Россия, Москва*"]);

        let bad = |k: &'static str, v: &'static str| {
            Config::default()
                .apply_env(&move |key| (key == k).then(|| v.to_string()))
                .is_err()
        };
        assert!(bad("MIHOMYAK_GATEWAY", "maybe"));
        assert!(bad("MIHOMYAK_MIXED_PORT", "99999"));
    }
}
