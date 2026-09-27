//! CLI command implementations.

mod core_cmd;
mod proxy;
mod subscription;

use std::process::ExitCode;

use anyhow::{Result, bail};

use crate::cli::{Cli, Command};
use crate::config::Config;
use crate::mihomo::api::Api;
use crate::subscription::ProviderInfo;
use crate::util::{fmt_bytes, fmt_date, fmt_duration, now_unix, sanitize};

pub fn run(cli: Cli) -> Result<ExitCode> {
    let mut config = Config::load(cli.config.as_deref(), cli.data_dir.as_deref())?;
    match cli.command {
        Command::Run => crate::service::supervisor::run(config).map(|()| ExitCode::SUCCESS),
        Command::Update => subscription::update(config),
        Command::Fetch {
            client,
            user_agent,
            body,
        } => {
            if let Some(client) = client {
                config.subscription.client = client;
            }
            if user_agent.is_some() {
                config.subscription.user_agent = user_agent;
            }
            subscription::fetch(config, body)
        }
        Command::Identity { client } => {
            if let Some(client) = client {
                config.subscription.client = client;
            }
            subscription::identity(config)
        }
        Command::Status => subscription::status(config),
        Command::Proxies { group } => proxy::proxies(&config, group.as_deref()),
        Command::Select { group, proxy } => proxy::select(&config, &group, &proxy),
        Command::Test {
            group,
            url,
            timeout,
        } => proxy::test(&config, group.as_deref(), &url, timeout),
        Command::Mode { mode } => proxy::mode_cmd(&config, mode.as_deref()),
        #[cfg(feature = "tui")]
        Command::Tui => crate::cli::tui::run(&config).map(|()| ExitCode::SUCCESS),
        Command::Render => subscription::render(config),
        Command::Check => subscription::check(config),
        Command::Health => Ok(core_cmd::health(&config)),
        Command::Core(cmd) => core_cmd::core_cmd(&config, cmd),
    }
}

fn api(config: &Config) -> Result<Api> {
    Api::from_config(config)
}

/// Exact, then case-insensitive, then unique substring match.
fn resolve_name<'a>(
    candidates: impl IntoIterator<Item = &'a String>,
    query: &str,
) -> Result<String> {
    let all: Vec<&String> = candidates.into_iter().collect();
    if let Some(exact) = all.iter().find(|c| c.as_str() == query) {
        return Ok((*exact).clone());
    }
    let q = query.to_lowercase();
    if let Some(ci) = all.iter().find(|c| c.to_lowercase() == q) {
        return Ok((*ci).clone());
    }
    let matches: Vec<&&String> = all
        .iter()
        .filter(|c| c.to_lowercase().contains(&q))
        .collect();
    match matches.as_slice() {
        [one] => Ok((**one).clone()),
        [] => bail!("no match for {query:?}"),
        many => bail!(
            "{query:?} is ambiguous: {}",
            many.iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn fmt_delay(delay: Option<&u32>) -> String {
    match delay {
        Some(&d) if d > 0 => format!("{d} ms"),
        Some(_) => "timeout".into(),
        None => "-".into(),
    }
}

fn print_provider(info: &ProviderInfo) {
    let row = |label: &str, value: &str| println!("{label:<13} {}", sanitize(value));
    if let Some(title) = &info.title {
        row("title:", title);
    }
    if let Some(u) = &info.usage {
        row(
            "traffic:",
            &format!("{} of {}", fmt_bytes(u.used()), u.total_display()),
        );
        if u.expire > 0 {
            row(
                "expires:",
                &format!(
                    "{} ({} days left)",
                    fmt_date(u.expire),
                    u.days_left(now_unix())
                ),
            );
        } else {
            row("expires:", "never");
        }
    }
    if let Some(d) = info.refill_date {
        row("traffic reset:", &fmt_date(d));
    }
    if let Some(i) = info.update_interval {
        row("interval:", &fmt_duration(i));
    }
    if let Some(s) = &info.support_url {
        row("support:", s);
    }
    if let Some(s) = &info.web_page_url {
        row("web page:", s);
    }
    if let Some(a) = &info.announce {
        row("announce:", a);
    }
    let h = info.hwid;
    if h.active || h.not_supported || h.max_devices_reached || h.limit {
        row(
            "hwid:",
            &format!(
                "active={} not-supported={} max-devices-reached={} limit={}",
                h.active, h.not_supported, h.max_devices_reached, h.limit
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_names_forgivingly() {
        let names: Vec<String> = ["🇳🇱 Netherlands", "🇩🇪 Germany", "AUTO"]
            .map(String::from)
            .to_vec();
        assert_eq!(resolve_name(&names, "AUTO").unwrap(), "AUTO");
        assert_eq!(resolve_name(&names, "auto").unwrap(), "AUTO");
        assert_eq!(resolve_name(&names, "nether").unwrap(), "🇳🇱 Netherlands");
        assert!(resolve_name(&names, "e").is_err(), "ambiguous");
        assert!(resolve_name(&names, "france").is_err());
    }
}
