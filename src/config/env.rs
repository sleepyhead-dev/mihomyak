//! `MIHOMYAK_*` environment variables, applied over the config file.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use super::Config;

impl Config {
    /// `env` is injected for testability.
    pub(super) fn apply_env(&mut self, env: &dyn Fn(&str) -> Option<String>) -> Result<()> {
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
        if let Some(v) = env("MIHOMYAK_PLATFORM") {
            sub.platform = v.parse()?;
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
        if let Some(v) = env("MIHOMYAK_DEVICE_SEED") {
            dev.seed = Some(v);
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
        if let Some(v) = flag("MIHOMYAK_ALLOW_DNS_LEAK")? {
            self.gateway.allow_dns_leak = v;
        }
        Ok(())
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
