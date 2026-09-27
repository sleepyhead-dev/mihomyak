//! Proxy related commands: proxies, select, test and mode.

use std::process::ExitCode;

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::util::sanitize;

pub(super) fn proxies(config: &Config, group: Option<&str>) -> Result<ExitCode> {
    let snapshot = super::api(config)?.snapshot()?;
    let Some(query) = group else {
        for g in &snapshot.groups {
            println!(
                "{:<24} {:<11} → {} ({} members)",
                sanitize(&g.name),
                g.kind,
                sanitize(g.now.as_deref().unwrap_or("-")),
                g.members.len()
            );
        }
        return Ok(ExitCode::SUCCESS);
    };
    let name = super::resolve_name(snapshot.groups.iter().map(|g| &g.name), query)?;
    let g = snapshot
        .groups
        .iter()
        .find(|g| g.name == name)
        .context("group vanished")?;
    println!("{} [{}]", sanitize(&g.name), g.kind);
    for member in &g.members {
        let mark = if g.now.as_deref() == Some(member) {
            "*"
        } else {
            " "
        };
        let kind = snapshot.kinds.get(member).map_or("", String::as_str);
        println!(
            " {mark} {:<32} {kind:<11} {}",
            sanitize(member),
            super::fmt_delay(snapshot.delays.get(member))
        );
    }
    Ok(ExitCode::SUCCESS)
}

pub(super) fn select(config: &Config, group: &str, proxy: &str) -> Result<ExitCode> {
    let api = super::api(config)?;
    let snapshot = api.snapshot()?;
    let group = super::resolve_name(snapshot.groups.iter().map(|g| &g.name), group)?;
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
    let proxy = super::resolve_name(g.members.iter(), proxy)?;
    api.select(&group, &proxy)?;
    println!("{} → {}", sanitize(&group), sanitize(&proxy));
    Ok(ExitCode::SUCCESS)
}

pub(super) fn test(
    config: &Config,
    group: Option<&str>,
    url: &str,
    timeout: u32,
) -> Result<ExitCode> {
    let api = super::api(config)?;
    let snapshot = api.snapshot()?;
    let groups: Vec<&crate::mihomo::api::Group> = match group {
        Some(q) => {
            let name = super::resolve_name(snapshot.groups.iter().map(|g| &g.name), q)?;
            snapshot.groups.iter().filter(|g| g.name == name).collect()
        }
        None => snapshot
            .groups
            .iter()
            .filter(|g| g.is_user_selector())
            .collect(),
    };
    for g in groups {
        println!("{}:", sanitize(&g.name));
        // mihomo's group-delay endpoint only reports members that answered the
        // probe in time, so a member missing from `delays` means it timed out.
        let delays = api.group_delay(&g.name, url, timeout)?;
        let mut rows: Vec<(&String, Option<&u32>)> =
            g.members.iter().map(|m| (m, delays.get(m))).collect();
        rows.sort_by_key(|(_, d)| d.map_or(u32::MAX, |d| *d));
        for (member, delay) in rows {
            println!(
                "  {:<32} {}",
                sanitize(member),
                super::fmt_delay(delay.or(Some(&0)))
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub(super) fn mode_cmd(config: &Config, mode: Option<&str>) -> Result<ExitCode> {
    let api = super::api(config)?;
    match mode {
        Some(mode) => {
            api.set_mode(mode)?;
            crate::service::store::Store::open(&config.data_dir)?.set_mode(mode)?;
            println!("mode: {mode}");
        }
        None => println!("mode: {}", api.mode()?),
    }
    Ok(ExitCode::SUCCESS)
}
