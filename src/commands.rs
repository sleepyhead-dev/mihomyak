//! CLI command implementations.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::api::Api;
use crate::cli::{Cli, Command, CoreCommand};
use crate::config::Config;
use crate::subscription::{self, ProviderInfo};
use crate::updater::{self, Outcome, Updater};
use crate::util::{fmt_bytes, fmt_date, fmt_duration, fmt_timestamp, now_unix};

pub fn run(cli: Cli) -> Result<ExitCode> {
    let mut config = Config::load(cli.config.as_deref(), cli.data_dir.as_deref())?;
    match cli.command {
        Command::Run => crate::supervisor::run(config).map(|()| ExitCode::SUCCESS),
        Command::Update => update(config),
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
            fetch(config, body)
        }
        Command::Identity { client } => {
            if let Some(client) = client {
                config.subscription.client = client;
            }
            identity(config)
        }
        Command::Status => status(config),
        Command::Proxies { group } => proxies(&config, group.as_deref()),
        Command::Select { group, proxy } => select(&config, &group, &proxy),
        Command::Test {
            group,
            url,
            timeout,
        } => test(&config, group.as_deref(), &url, timeout),
        Command::Mode { mode } => mode_cmd(&config, mode.as_deref()),
        #[cfg(feature = "tui")]
        Command::Tui => crate::tui::run(&config).map(|()| ExitCode::SUCCESS),
        Command::Health => Ok(match api(&config).and_then(|a| a.version()) {
            Ok(_) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("unhealthy: {e:#}");
                ExitCode::FAILURE
            }
        }),
        Command::Core(cmd) => core_cmd(&config, cmd),
    }
}

/// API client for the configured controller (secret from config or data dir).
pub fn api(config: &Config) -> Result<Api> {
    let secret = match &config.core.secret {
        Some(secret) => secret.clone(),
        None => crate::store::Store::open(&config.data_dir)?.secret()?,
    };
    Api::new(&config.core.controller, &secret)
}

fn update(config: Config) -> Result<ExitCode> {
    let updater = Updater::new(config)?;
    if let Some(pid) = updater.store.supervisor_pid() {
        let before = updater.store.load_meta().map_or(0, |m| m.checked_at);
        // SAFETY: kill(2) with a pid read from our own pid file, verified alive.
        if unsafe { libc::kill(pid, libc::SIGHUP) } != 0 {
            bail!(
                "cannot signal the supervisor (pid {pid}): {}",
                std::io::Error::last_os_error()
            );
        }
        println!("asked the supervisor (pid {pid}) to update…");
        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(300));
            let Some(meta) = updater.store.load_meta() else {
                continue;
            };
            if meta.checked_at > before {
                return Ok(match meta.last_error {
                    None => {
                        println!("updated ({} proxies)", meta.proxies);
                        ExitCode::SUCCESS
                    }
                    Some(error) => {
                        println!("not applied: {error}");
                        ExitCode::FAILURE
                    }
                });
            }
        }
        bail!("the supervisor did not report back within 2 minutes; check its logs");
    }
    match updater.update()? {
        Outcome::Applied {
            changed,
            info,
            proxies,
        } => {
            println!(
                "{}: {}",
                if changed { "updated" } else { "unchanged" },
                updater::describe(&info, proxies)
            );
            // A core started outside the supervisor can still pick it up.
            if let Ok(api) = Api::new(&updater.config.core.controller, &updater.secret)
                && api.reload(&updater.store.mihomo_config()).is_ok()
            {
                println!("mihomo reloaded");
            }
            Ok(ExitCode::SUCCESS)
        }
        Outcome::Kept(problem) => {
            println!("not applied: {problem}");
            Ok(ExitCode::FAILURE)
        }
    }
}

fn fetch(config: Config, show_body: bool) -> Result<ExitCode> {
    let updater = Updater::new(config)?;
    let url = updater.url()?;
    println!(
        "client: {} ({})",
        updater.emulation.kind,
        updater.emulation.user_agent()
    );
    let (fetch, analysis) = updater.fetch()?;
    for hop in &fetch.hops {
        println!("redirected from {}", subscription::redact(hop));
    }
    println!("\n> GET {} HTTP/1.1", subscription::redact(&url));
    for (name, value) in &fetch.request_headers {
        println!("> {name}: {value}");
    }
    let response = &fetch.response;
    println!("\n< HTTP/1.1 {} {}", response.status, response.reason);
    for (name, value) in &response.headers {
        println!("< {name}: {value}");
    }
    println!("< ({} bytes)", response.body.len());

    println!();
    print_provider(&analysis.info);
    if let Some(content) = &analysis.content {
        println!(
            "format:       {} ({} proxies)",
            content.format.as_str(),
            content.endpoints.len()
        );
        for e in content.endpoints.iter().take(15) {
            println!("              - {} ({}:{})", e.name, e.server, e.port);
        }
        for note in &content.notes {
            println!("              ! {note}");
        }
        if content.endpoints.len() > 15 {
            println!("              … {} more", content.endpoints.len() - 15);
        }
    }
    let usable = analysis
        .usable(updater.config.subscription.accept_stub)
        .is_some();
    match &analysis.problem {
        None => println!("verdict:      OK, usable"),
        Some(p) => println!(
            "verdict:      {} ({p})",
            if usable { "STUB, accepted" } else { "REJECTED" }
        ),
    }
    if show_body {
        println!("\n{}", String::from_utf8_lossy(&response.body));
    }
    Ok(if usable {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn print_provider(info: &ProviderInfo) {
    let row = |label: &str, value: &str| println!("{label:<13} {value}");
    if let Some(title) = &info.title {
        row("title:", title);
    }
    if let Some(u) = &info.usage {
        let total = if u.total == 0 {
            "unlimited".into()
        } else {
            fmt_bytes(u.total)
        };
        row("traffic:", &format!("{} of {total}", fmt_bytes(u.used())));
        if u.expire > 0 {
            let left = u.expire.saturating_sub(now_unix()) / 86_400;
            row(
                "expires:",
                &format!("{} ({left} days left)", fmt_date(u.expire)),
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

fn identity(config: Config) -> Result<ExitCode> {
    let updater = Updater::new(config)?;
    let e = &updater.emulation;
    let d = e.device_headers();
    println!("client:        {}", e.kind);
    println!("user-agent:    {}", e.user_agent());
    println!("hwid:          {}", d.hwid);
    println!("device os:     {}", d.os);
    println!("os version:    {}", d.os_version.as_deref().unwrap_or("-"));
    println!("device model:  {}", d.model);
    println!("data dir:      {}", updater.store.root().display());
    let url = updater
        .url()
        .or_else(|_| crate::http::Url::parse("https://sub.example.com/token"))?;
    println!("\nrequest headers for {}:", subscription::redact(&url));
    for (name, value) in e.headers(&url) {
        println!("  {name}: {value}");
    }
    if !crate::emulation::is_valid_hwid(&d.hwid) {
        println!("\nwarning: this HWID fails Remnawave's ^[a-zA-Z0-9=-]{{10,64}}$ check");
    }
    Ok(ExitCode::SUCCESS)
}

fn status(config: Config) -> Result<ExitCode> {
    let store = crate::store::Store::open(&config.data_dir)?;
    match store.load_meta() {
        Some(meta) => {
            let info = ProviderInfo::from_headers(&meta.headers);
            println!("subscription");
            print_provider(&info);
            if meta.fetched_at > 0 {
                println!(
                    "{:<13} {} ({} proxies)",
                    "format:", meta.format, meta.proxies
                );
                println!(
                    "{:<13} {} ({} ago)",
                    "fetched:",
                    fmt_timestamp(meta.fetched_at),
                    fmt_duration(Duration::from_secs(
                        now_unix().saturating_sub(meta.fetched_at)
                    ))
                );
            }
            if let Some(error) = &meta.last_error {
                println!(
                    "{:<13} {} at {}",
                    "last error:",
                    error,
                    fmt_timestamp(meta.checked_at)
                );
            }
        }
        None => println!("subscription: never fetched"),
    }
    println!();
    match store.supervisor_pid() {
        Some(pid) => println!("supervisor:   running (pid {pid})"),
        None => println!("supervisor:   not running"),
    }
    let api = api(&config)?;
    match api.version() {
        Ok(version) => {
            println!("mihomo:       {version}");
            if let Ok(mode) = api.mode() {
                println!("mode:         {mode}");
            }
            if let Ok((up, down, conns)) = api.traffic() {
                println!(
                    "traffic:      ↑ {} ↓ {} ({conns} connections)",
                    fmt_bytes(up),
                    fmt_bytes(down)
                );
            }
            if let Ok(snapshot) = api.snapshot() {
                for g in snapshot
                    .groups
                    .iter()
                    .filter(|g| g.selectable() && g.name != "GLOBAL")
                {
                    println!(
                        "{:<13} {}",
                        format!("{}:", g.name),
                        g.now.as_deref().unwrap_or("-")
                    );
                }
            }
        }
        Err(e) => println!("mihomo:       unreachable ({e:#})"),
    }
    Ok(ExitCode::SUCCESS)
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

fn proxies(config: &Config, group: Option<&str>) -> Result<ExitCode> {
    let snapshot = api(config)?.snapshot()?;
    let Some(query) = group else {
        for g in &snapshot.groups {
            println!(
                "{:<24} {:<11} → {} ({} members)",
                g.name,
                g.kind,
                g.now.as_deref().unwrap_or("-"),
                g.members.len()
            );
        }
        return Ok(ExitCode::SUCCESS);
    };
    let name = resolve_name(snapshot.groups.iter().map(|g| &g.name), query)?;
    let g = snapshot
        .groups
        .iter()
        .find(|g| g.name == name)
        .context("group vanished")?;
    println!("{} [{}]", g.name, g.kind);
    for member in &g.members {
        let mark = if g.now.as_deref() == Some(member) {
            "*"
        } else {
            " "
        };
        let kind = snapshot.kinds.get(member).map_or("", String::as_str);
        println!(
            " {mark} {member:<32} {kind:<11} {}",
            fmt_delay(snapshot.delays.get(member))
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn select(config: &Config, group: &str, proxy: &str) -> Result<ExitCode> {
    let api = api(config)?;
    let snapshot = api.snapshot()?;
    let group = resolve_name(snapshot.groups.iter().map(|g| &g.name), group)?;
    let g = snapshot
        .groups
        .iter()
        .find(|g| g.name == group)
        .context("group vanished")?;
    if !g.selectable() {
        bail!(
            "{group} is a {} group; only Selector groups accept a choice",
            g.kind
        );
    }
    let proxy = resolve_name(g.members.iter(), proxy)?;
    api.select(&group, &proxy)?;
    println!("{group} → {proxy}");
    Ok(ExitCode::SUCCESS)
}

fn test(config: &Config, group: Option<&str>, url: &str, timeout: u32) -> Result<ExitCode> {
    let api = api(config)?;
    let snapshot = api.snapshot()?;
    let groups: Vec<&crate::api::Group> = match group {
        Some(q) => {
            let name = resolve_name(snapshot.groups.iter().map(|g| &g.name), q)?;
            snapshot.groups.iter().filter(|g| g.name == name).collect()
        }
        None => snapshot
            .groups
            .iter()
            .filter(|g| g.selectable() && g.name != "GLOBAL")
            .collect(),
    };
    for g in groups {
        println!("{}:", g.name);
        let delays = api.group_delay(&g.name, url, timeout)?;
        let mut rows: Vec<(&String, Option<&u32>)> =
            g.members.iter().map(|m| (m, delays.get(m))).collect();
        rows.sort_by_key(|(_, d)| d.map_or(u32::MAX, |d| *d));
        for (member, delay) in rows {
            println!("  {member:<32} {}", fmt_delay(delay.or(Some(&0))));
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn mode_cmd(config: &Config, mode: Option<&str>) -> Result<ExitCode> {
    let api = api(config)?;
    match mode {
        Some(mode) => {
            api.set_mode(mode)?;
            crate::store::Store::open(&config.data_dir)?.set_mode(mode)?;
            println!("mode: {mode}");
        }
        None => println!("mode: {}", api.mode()?),
    }
    Ok(ExitCode::SUCCESS)
}

fn core_cmd(config: &Config, cmd: CoreCommand) -> Result<ExitCode> {
    let store = crate::store::Store::open(&config.data_dir)?;
    match cmd {
        CoreCommand::Install {
            version,
            dest,
            mirror,
        } => {
            let dest = dest.unwrap_or_else(|| store.root().join("bin/mihomo"));
            let tag = crate::core::install(version.as_deref(), &dest, mirror.as_deref())?;
            println!("installed mihomo {tag} to {}", dest.display());
        }
        CoreCommand::Version => {
            let bin = crate::core::resolve_bin(config, &store);
            println!("{} ({})", crate::core::version(&bin)?, bin.display());
        }
    }
    Ok(ExitCode::SUCCESS)
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
