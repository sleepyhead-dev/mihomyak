//! Checks of the combined settings, run once the file and the environment are applied.

use anyhow::{Context, Result, bail};

use super::{Config, is_mode};

impl Config {
    pub(super) fn validate(&self) -> Result<()> {
        if self.device.machine_id.is_some() && self.device.seed.is_some() {
            bail!("set either device.machine_id or device.seed, not both");
        }
        if let Some(seed) = &self.device.seed
            && seed.trim().is_empty()
        {
            bail!("device.seed is empty");
        }
        if let Some(url) = &self.subscription.url {
            crate::client::http::Url::parse(url).context("subscription.url")?;
        }
        if let Some(proxy) = &self.subscription.proxy {
            let url = crate::client::http::Url::parse(proxy).context("subscription.proxy")?;
            if url.scheme != crate::client::http::Scheme::Http {
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
            cron.parse::<crate::service::schedule::Cron>()?;
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
}
