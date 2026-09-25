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
    /// Overrides the emulated mihomo core version shown in FlClashX's User-Agent.
    pub core_version: Option<String>,
    /// Replaces the User-Agent value (keeps the emulated header order).
    pub user_agent: Option<String>,
    /// Extra `Name: value` headers; a name that already exists is replaced in place.
    pub headers: Vec<String>,
    /// `auto` (provider's `profile-update-interval`, else 24h) or a duration like `12h`.
    pub interval: Interval,
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
            core_version: None,
            user_agent: None,
            headers: Vec::new(),
            interval: Interval::Auto,
            proxy: None,
            accept_stub: false,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Interval {
    #[default]
    Auto,
    Fixed(Duration),
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
        let d = crate::util::parse_duration(s)?;
        if d < Duration::from_secs(60) {
            bail!("update interval {s:?} is shorter than one minute");
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
    pub mixed_port: u16,
    pub allow_lan: bool,
    pub bind_address: String,
    pub log_level: String,
    /// Default routing mode (`rule`, `global`, `direct`); `mihomyak mode` persists changes.
    pub mode: String,
    /// Go runtime soft memory limit for mihomo (`GOMEMLIMIT`, e.g. `64MiB`).
    pub memory_limit: Option<String>,
}

impl Default for Core {
    fn default() -> Self {
        Self {
            bin: PathBuf::from("mihomo"),
            controller: "127.0.0.1:9090".into(),
            secret: None,
            mixed_port: 7890,
            allow_lan: true,
            bind_address: "*".into(),
            log_level: "warning".into(),
            mode: "rule".into(),
            memory_limit: None,
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
    /// Where mihomo's DNS server listens.
    pub dns_listen: String,
}

impl Default for Gateway {
    fn default() -> Self {
        Self {
            enable: false,
            stack: "system".into(),
            auto_redirect: false,
            dns_listen: "0.0.0.0:1053".into(),
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
        config.source = Some(path.to_path_buf());
        Ok(config)
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
        if let Some(v) = env("MIHOMYAK_UPDATE_INTERVAL") {
            sub.interval = v.parse().context("MIHOMYAK_UPDATE_INTERVAL")?;
        }
        if let Some(v) = env("MIHOMYAK_FETCH_PROXY") {
            sub.proxy = Some(v).filter(|s| !s.is_empty());
        }
        if let Some(v) = flag("MIHOMYAK_ACCEPT_STUB")? {
            sub.accept_stub = v;
        }
        let dev = &mut self.device;
        if let Some(v) = env("MIHOMYAK_MACHINE_ID") {
            dev.machine_id = Some(v);
        }
        if let Some(v) = env("MIHOMYAK_HWID") {
            dev.hwid = Some(v);
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
        if let Some(v) = env("MIHOMYAK_MIXED_PORT") {
            core.mixed_port = v.parse().context("MIHOMYAK_MIXED_PORT")?;
        }
        if let Some(v) = env("MIHOMYAK_MODE") {
            core.mode = v;
        }
        if let Some(v) = env("MIHOMYAK_LOG_LEVEL") {
            core.log_level = v;
        }
        if let Some(v) = env("MIHOMYAK_MEMORY_LIMIT") {
            core.memory_limit = Some(v).filter(|s| !s.is_empty());
        }
        if let Some(v) = flag("MIHOMYAK_GATEWAY")? {
            self.gateway.enable = v;
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
            if !header.contains(':') {
                bail!("subscription.headers entry {header:?} must look like \"Name: value\"");
            }
        }
        if !is_mode(&self.core.mode) {
            bail!("core.mode must be rule, global or direct");
        }
        if !["system", "gvisor", "mixed"].contains(&self.gateway.stack.as_str()) {
            bail!("gateway.stack must be system, gvisor or mixed");
        }
        Ok(())
    }

    pub fn subscription_url(&self) -> Result<&str> {
        self.subscription.url.as_deref().filter(|u| !u.is_empty()).context(
            "no subscription URL: set MIHOMYAK_SUB_URL or [subscription] url in the config file",
        )
    }
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
    fn parses_full_file() {
        let config: Config = toml::from_str(
            r#"
            [subscription]
            url = "https://sub.example.com/abc"
            client = "koala"
            interval = "6h"
            headers = ["X-Extra: 1"]

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
            config.subscription.interval,
            Interval::Fixed(Duration::from_secs(6 * 3600))
        );
        assert_eq!(config.core.mixed_port, 7891);
        assert!(config.gateway.enable);
        assert_eq!(config.mihomo["ipv6"].as_bool(), Some(true));
        config.validate().unwrap();
    }

    #[test]
    fn rejects_unknown_keys_and_bad_values() {
        assert!(toml::from_str::<Config>("[subscription]\nurll = \"x\"").is_err());
        assert!(toml::from_str::<Config>("[subscription]\ninterval = \"10s\"").is_err());
        assert!(toml::from_str::<Config>("[subscription]\nclient = \"happ\"").is_err());
    }

    #[test]
    fn env_overrides_file() {
        let mut config = Config::default();
        let env: HashMap<&str, &str> = [
            ("MIHOMYAK_SUB_URL", "https://s.example/x"),
            ("MIHOMYAK_CLIENT", "koala"),
            ("MIHOMYAK_GATEWAY", "true"),
            ("MIHOMYAK_UPDATE_INTERVAL", "auto"),
            ("MIHOMYAK_MIXED_PORT", "1080"),
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
        assert!(config.gateway.enable);
        assert_eq!(config.core.mixed_port, 1080);

        let bad = |k: &'static str, v: &'static str| {
            Config::default()
                .apply_env(&move |key| (key == k).then(|| v.to_string()))
                .is_err()
        };
        assert!(bad("MIHOMYAK_GATEWAY", "maybe"));
        assert!(bad("MIHOMYAK_MIXED_PORT", "99999"));
    }
}
