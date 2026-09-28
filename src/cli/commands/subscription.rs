//! Subscription and config related commands: update, fetch, identity,
//! status, check and render.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::config::Config;
use crate::mihomo::api::Api;
use crate::service::updater::{self, Outcome, Updater};
use crate::subscription::{self, ProviderInfo};
use crate::util::{fmt_bytes, fmt_duration, fmt_timestamp, now_unix, sanitize};

pub(super) fn update(config: Config) -> Result<ExitCode> {
    let updater = Updater::new(config)?;
    let before = updater.store.load_meta().map_or(0, |m| m.update_seq);
    if let Some(pid) = updater.store.signal_supervisor(libc::SIGHUP)? {
        println!("asked the supervisor (pid {pid}) to update…");
        let deadline = Instant::now() + Duration::from_secs(180);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(300));
            let Some(meta) = updater.store.load_meta() else {
                continue;
            };
            if meta.update_seq != before {
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
        bail!("the supervisor did not report back within 3 minutes; check its logs");
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
            if changed
                && let Ok(api) = Api::new(&updater.config.core.controller, &updater.secret)
                && api.version().is_ok()
            {
                match api.reload(&updater.store.mihomo_config()) {
                    Ok(()) => println!("mihomo reloaded"),
                    Err(e) => {
                        updater.rollback(&format!("{e:#}"))?;
                        println!("mihomo rejected the new config, previous one restored: {e:#}");
                        return Ok(ExitCode::FAILURE);
                    }
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Outcome::Kept(problem) => {
            println!("not applied: {problem}");
            Ok(ExitCode::FAILURE)
        }
    }
}

pub(super) fn fetch(config: Config, show_body: bool) -> Result<ExitCode> {
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
    // Everything below comes from the server: strip terminal control sequences.
    let response = &fetch.response;
    println!(
        "\n< HTTP/1.1 {} {}",
        response.status,
        sanitize(&response.reason)
    );
    for (name, value) in &response.headers {
        println!("< {}: {}", sanitize(name), sanitize(value));
    }
    println!("< ({} bytes)", response.body.len());

    println!();
    super::print_provider(&analysis.info);
    if let Some(content) = &analysis.content {
        println!(
            "format:       {} ({} proxies)",
            content.format.as_str(),
            content.endpoints.len()
        );
        for e in content.endpoints.iter().take(15) {
            println!(
                "              - {} ({}:{})",
                sanitize(&e.name),
                sanitize(&e.server),
                e.port
            );
        }
        for note in &content.notes {
            println!("              ! {}", sanitize(note));
        }
        if content.endpoints.len() > 15 {
            println!("              … {} more", content.endpoints.len() - 15);
        }
    }
    let usable = analysis.usable().is_some();
    match &analysis.problem {
        None => println!("verdict:      OK, usable"),
        Some(p) => println!("verdict:      REJECTED ({})", sanitize(p.message())),
    }
    if show_body {
        println!();
        for line in String::from_utf8_lossy(&response.body).lines() {
            println!("{}", sanitize(line));
        }
    }
    Ok(if usable {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

pub(super) fn check(config: Config) -> Result<ExitCode> {
    println!(
        "config:       {}",
        config
            .source
            .as_ref()
            .map_or("(env/defaults only)".into(), |p| p.display().to_string())
    );
    let updater = Updater::new(config)?;
    let cfg = &updater.config;
    println!(
        "client:       {} on {} ({})",
        updater.emulation.kind,
        updater.emulation.platform,
        updater.emulation.user_agent()
    );
    println!("subscription: {}", subscription::redact(&updater.url()?));
    println!(
        "update:       interval {}, cron {:?}, on start: {}",
        cfg.update.interval, cfg.update.cron, cfg.update.on_start
    );
    println!(
        "filter:       include {:?}, exclude {:?}",
        cfg.filter.include, cfg.filter.exclude
    );
    for g in &cfg.groups {
        println!(
            "group:        {} ({}) nodes {:?}{}",
            g.name,
            g.kind.as_mihomo(),
            g.nodes,
            if g.default { ", default" } else { "" }
        );
    }
    println!(
        "proxy:        port {}, lan {}, auth {}",
        cfg.core.mixed_port,
        if cfg.core.allow_lan {
            "allowed (private ranges)"
        } else {
            "off"
        },
        if cfg.core.auth.is_empty() {
            "off"
        } else {
            "on"
        }
    );
    println!(
        "gateway:      {}",
        match (cfg.gateway.enable, cfg.gateway.kill_switch) {
            (false, _) => "off",
            (true, false) => "on (TUN)",
            (true, true) => "on (TUN, kill switch)",
        }
    );
    if cfg.gateway.enable
        && let Some(warning) = crate::gateway::dns_leak()
    {
        println!("warning:      {warning}");
    }
    let built = match updater.render() {
        Ok(built) => built,
        Err(e) => {
            println!("render:       skipped ({e:#})");
            return Ok(ExitCode::SUCCESS);
        }
    };
    for warning in &built.warnings {
        println!("warning:      {}", sanitize(warning));
    }
    let bin = crate::mihomo::core::resolve_bin(cfg, &updater.store);
    match updater.check_config(&built) {
        Ok(None) => {
            println!("mihomo -t:    ok");
            Ok(ExitCode::SUCCESS)
        }
        Ok(Some(log)) => {
            println!("mihomo -t:    FAILED");
            for line in log.lines() {
                println!("{}", sanitize(line));
            }
            Ok(ExitCode::FAILURE)
        }
        Err(e) => {
            println!("mihomo -t:    skipped ({}: {e:#})", bin.display());
            Ok(ExitCode::SUCCESS)
        }
    }
}

pub(super) fn identity(config: Config) -> Result<ExitCode> {
    let updater = Updater::new(config)?;
    let e = &updater.emulation;
    let d = e.device_headers();
    println!("client:        {} on {}", e.kind, e.platform);
    println!("user-agent:    {}", e.user_agent());
    println!("hwid:          {}", d.hwid);
    println!("device os:     {}", d.os);
    println!("os version:    {}", d.os_version.as_deref().unwrap_or("-"));
    println!("device model:  {}", d.model);
    println!("data dir:      {}", updater.store.root().display());
    let url = updater
        .url()
        .or_else(|_| crate::client::http::Url::parse("https://sub.example.com/token"))?;
    println!("\nrequest headers for {}:", subscription::redact(&url));
    for (name, value) in e.headers(&url) {
        println!("  {name}: {value}");
    }
    if !crate::client::emulation::is_valid_hwid(&d.hwid) {
        println!("\nwarning: this HWID fails Remnawave's ^[a-zA-Z0-9=-]{{10,64}}$ check");
    }
    Ok(ExitCode::SUCCESS)
}

pub(super) fn status(config: Config) -> Result<ExitCode> {
    let store = crate::service::store::Store::open(&config.data_dir)?;
    match store.load_meta() {
        Some(meta) => {
            let info = ProviderInfo::from_headers(&meta.headers);
            println!("subscription");
            super::print_provider(&info);
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
            if let (Some(at), Some(_)) = (meta.next_update_at, store.supervisor_pid()) {
                let when = if at <= now_unix() {
                    "now".to_owned()
                } else {
                    format!(
                        "{} (in {})",
                        fmt_timestamp(at),
                        fmt_duration(Duration::from_secs(at - now_unix()))
                    )
                };
                println!("{:<13} {when}", "next update:");
            }
            if let Some(error) = &meta.last_error {
                println!(
                    "{:<13} {} at {}",
                    "last error:",
                    sanitize(error),
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
    let api = super::api(&config)?;
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
                for g in snapshot.groups.iter().filter(|g| g.is_user_selector()) {
                    println!(
                        "{:<13} {}",
                        format!("{}:", sanitize(&g.name)),
                        sanitize(g.now.as_deref().unwrap_or("-"))
                    );
                }
            }
        }
        Err(e) => println!("mihomo:       unreachable ({e:#})"),
    }
    Ok(ExitCode::SUCCESS)
}

pub(super) fn render(config: Config) -> Result<ExitCode> {
    let built = Updater::new(config)?.render()?;
    print!("{}", built.config_yaml);
    for warning in &built.warnings {
        eprintln!("warning: {warning}");
    }
    Ok(ExitCode::SUCCESS)
}
