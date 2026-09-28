//! Command-line interface: clap definitions plus the styled, grouped help.
//!
//! clap cannot group subcommands under headings, so the top-level help is a
//! template assembled from the subcommands' own descriptions (`GROUPS`); a
//! test makes sure every command is listed.

pub mod commands;
#[cfg(feature = "tui")]
pub mod tui;

use std::fmt::Write as _;
use std::path::PathBuf;

use clap::builder::styling::{AnsiColor, Effects, Style, Styles};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};

use crate::client::emulation::{ClientKind, Platform};

#[derive(Debug, Parser)]
#[command(
    name = "mihomyak",
    version,
    about = "Lightweight mihomo supervisor for CIS VPN subscriptions: impersonates \
             FlClashX, Koala Clash or Happ, keeps mihomo running and can serve as a \
             transparent gateway for Docker containers.",
    after_help = EXAMPLES,
)]
pub struct Cli {
    /// Config file [default: $MIHOMYAK_CONFIG, ~/.config/mihomyak/config.toml,
    /// /etc/mihomyak/config.toml for root]
    #[arg(short, long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Data directory: HWID seed, secrets, cache [default: $MIHOMYAK_DATA_DIR,
    /// ~/.local/share/mihomyak, /var/lib/mihomyak for root]
    #[arg(short, long, global = true, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

const EXAMPLES: &str = "\
Examples:
  mihomyak run                         supervise mihomo (the container entry point)
  mihomyak status                      traffic, expiry, next update, chosen proxies
  mihomyak select PROXY nl             names can be shortened to a unique part
  mihomyak fetch --client happ         what does the panel send to Happ?
  docker exec -it mihomyak mihomyak tui

Settings live in config.toml or MIHOMYAK_* variables (docs/CONFIG.md).
Run 'mihomyak <command> --help' for details on a command.";

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Supervise mihomo in the foreground and keep the subscription fresh.
    #[command(
        long_about = "Supervise mihomo in the foreground and keep the subscription fresh.\n\n\
            Starts from the cached subscription when there is one (the network is up at \
            once), updates on the configured schedule (interval, cron, on start), \
            validates every new config with `mihomo -t`, hot-reloads it and restarts \
            mihomo with backoff if it dies. In a container it is PID 1.",
        after_help = "Signals:\n  SIGHUP   update the subscription now (what `mihomyak update` sends)\n  \
            SIGTERM  stop mihomo gracefully and exit"
    )]
    Run,
    /// Update the subscription now.
    #[command(long_about = "Update the subscription now.\n\n\
            With a running supervisor it asks it to update and waits for the result; \
            otherwise it fetches and writes the config itself (and hot-reloads a \
            mihomo started elsewhere). Exits 1 if the response was not applied \
            (stub, device limit, HTTP error).")]
    Update,
    /// Fetch and analyse the subscription without applying it.
    #[command(
        long_about = "Fetch and analyse the subscription without applying it.\n\n\
            Prints the exact request headers, the panel's response headers, provider \
            info, the detected format and proxies, and a verdict (usable, stub, \
            refused). Nothing is written.",
        after_help = "Examples:\n  mihomyak fetch\n  mihomyak fetch --client happ --platform windows\n  \
            mihomyak fetch --user-agent 'clash-verge/v2.4.0' --body"
    )]
    Fetch {
        /// Impersonate another client for this request.
        #[arg(long, value_name = "CLIENT", value_parser = parse_client)]
        client: Option<ClientKind>,
        /// Impersonate the client on another platform (linux, windows, android).
        #[arg(long, value_name = "OS", value_parser = parse_platform)]
        platform: Option<Platform>,
        /// Override the User-Agent for this request.
        #[arg(long, value_name = "UA")]
        user_agent: Option<String>,
        /// Print the response body.
        #[arg(long)]
        body: bool,
    },
    /// Show the emulated device, its HWID and the exact request headers.
    Identity {
        /// Show what another client would send.
        #[arg(long, value_name = "CLIENT", value_parser = parse_client)]
        client: Option<ClientKind>,
        /// Show what the client would send on another platform.
        #[arg(long, value_name = "OS", value_parser = parse_platform)]
        platform: Option<Platform>,
    },
    /// Traffic, expiry, next update, mihomo state and chosen proxies.
    Status,
    /// List proxy groups, or the proxies of one group with delays.
    #[command(visible_alias = "groups")]
    Proxies {
        /// Group name: exact, case-insensitive or a unique part of it.
        group: Option<String>,
    },
    /// Choose a proxy in a selector group.
    #[command(after_help = "Examples:\n  mihomyak select PROXY 'NL Amsterdam'\n  \
        mihomyak select prox nl          unique parts are enough")]
    Select {
        /// Group name: exact, case-insensitive or a unique part of it.
        group: String,
        /// Proxy name, matched the same way.
        proxy: String,
    },
    /// Measure proxy delays (every selector group, or one group).
    Test {
        /// Group name: exact, case-insensitive or a unique part of it.
        group: Option<String>,
        /// URL requested through each proxy.
        #[arg(long, default_value = crate::mihomo::profile::HEALTH_CHECK_URL)]
        url: String,
        /// Per-proxy timeout.
        #[arg(long, default_value_t = crate::mihomo::api::DELAY_TIMEOUT_MS, value_name = "MS")]
        timeout: u32,
    },
    /// Show or switch the routing mode (rule, global, direct).
    Mode {
        /// New mode (omit to show the current one).
        #[arg(value_parser = ["rule", "global", "direct"])]
        mode: Option<String>,
    },
    /// Interactive terminal UI: groups, proxies, delays, mode.
    #[cfg(feature = "tui")]
    Tui,
    /// Print the mihomo config generated from the cached subscription.
    Render,
    /// Validate the settings and the generated config with `mihomo -t`.
    Check,
    /// Exit 0 if mihomo's API answers (container health check).
    Health,
    /// Install or inspect the mihomo binary.
    #[command(subcommand)]
    Core(CoreCommand),
}

/// Top-level help sections; every subcommand must appear exactly once.
const GROUPS: &[(&str, &[&str])] = &[
    ("Service", &["run", "update", "status"]),
    ("Proxies", &["proxies", "select", "test", "mode", "tui"]),
    (
        "Diagnostics",
        &["fetch", "identity", "check", "render", "health"],
    ),
    ("Maintenance", &["core"]),
];

/// Colours in the style of cargo; clap drops them for pipes and `NO_COLOR`.
fn styles() -> Styles {
    Styles::styled()
        .header(AnsiColor::BrightGreen.on_default().effects(Effects::BOLD))
        .usage(AnsiColor::BrightGreen.on_default().effects(Effects::BOLD))
        .literal(AnsiColor::BrightCyan.on_default().effects(Effects::BOLD))
        .placeholder(AnsiColor::Cyan.on_default())
        .error(AnsiColor::BrightRed.on_default().effects(Effects::BOLD))
        .valid(AnsiColor::BrightCyan.on_default().effects(Effects::BOLD))
        .invalid(AnsiColor::Yellow.on_default().effects(Effects::BOLD))
}

/// The full clap command: derive definitions plus styles, grouped help and a
/// long version that names the emulated clients.
pub fn command() -> clap::Command {
    let cmd = Cli::command().styles(styles());
    let header: Style = *cmd.get_styles().get_header();
    let literal: Style = *cmd.get_styles().get_literal();
    let mut commands = String::new();
    for (title, names) in GROUPS {
        let _ = writeln!(commands, "{header}{title}:{header:#}");
        for name in names.iter() {
            let Some(sub) = cmd.find_subcommand(name) else {
                continue; // `tui` without the feature
            };
            let about = sub.get_about().map(ToString::to_string).unwrap_or_default();
            let _ = writeln!(commands, "  {literal}{name:<10}{literal:#}{about}");
        }
        commands.push('\n');
    }
    let template = format!(
        "{{before-help}}{{about-with-newline}}\n{{usage-heading}} {{usage}}\n\n\
         {commands}{header}Options:{header:#}\n{{options}}{{after-help}}"
    );
    let long_version = format!(
        "{}\nemulates  FlClashX {} (core {}), Koala Clash {}, Happ {}\ntarget    {}-linux",
        env!("CARGO_PKG_VERSION"),
        crate::client::emulation::FLCLASHX_VERSION,
        crate::client::emulation::FLCLASHX_CORE_VERSION,
        crate::client::emulation::KOALA_VERSION,
        crate::client::emulation::HAPP_VERSION,
        std::env::consts::ARCH,
    );
    // Built once per process; clap wants a `'static` string here.
    let long_version: &'static str = Box::leak(long_version.into_boxed_str());
    style_after_help(cmd.help_template(template), header).long_version(long_version)
}

/// Gives the `Examples:`/`Signals:` headings of every after-help the same colour
/// as clap's own headings.
fn style_after_help(mut cmd: clap::Command, header: Style) -> clap::Command {
    if let Some(text) = cmd.get_after_help().map(ToString::to_string) {
        let styled: String = text
            .lines()
            .map(|line| {
                if !line.starts_with(' ') && line.ends_with(':') {
                    format!("{header}{line}{header:#}\n")
                } else {
                    format!("{line}\n")
                }
            })
            .collect();
        cmd = cmd.after_help(styled.trim_end().to_owned());
    }
    let names: Vec<String> = cmd
        .get_subcommands()
        .map(|s| s.get_name().to_owned())
        .collect();
    for name in names {
        cmd = cmd.mut_subcommand(name, |sub| style_after_help(sub, header));
    }
    cmd
}

/// Parses `std::env::args`, printing help/errors and exiting like `Parser::parse`.
pub fn parse() -> Cli {
    let matches = command().get_matches();
    Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit())
}

#[derive(Debug, Subcommand)]
pub enum CoreCommand {
    /// Download mihomo from GitHub releases for this CPU.
    #[command(after_help = "Examples:\n  mihomyak core install\n  \
        sudo mihomyak core install --version v1.19.31 --dest /usr/local/bin/mihomo\n  \
        mihomyak core install --mirror https://mirror.example --sha256 <hex>")]
    Install {
        /// Release tag, e.g. v1.19.31 (default: latest).
        #[arg(long)]
        version: Option<String>,
        /// Destination path (default: DATA_DIR/bin/mihomo).
        #[arg(long)]
        dest: Option<PathBuf>,
        /// Base URL replacing github.com for downloads (mirrors for restricted networks).
        #[arg(long, env = "MIHOMYAK_GITHUB_MIRROR")]
        mirror: Option<String>,
        /// Expected SHA-256 of the downloaded .gz (required with --mirror).
        #[arg(long, value_name = "HEX")]
        sha256: Option<String>,
    },
    /// Print the version of the configured mihomo binary.
    Version,
}

fn parse_platform(s: &str) -> Result<Platform, String> {
    s.parse().map_err(|e: anyhow::Error| e.to_string())
}

fn parse_client(s: &str) -> Result<ClientKind, String> {
    s.parse().map_err(|e: anyhow::Error| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definitions_are_consistent() {
        command().debug_assert();
    }

    #[test]
    fn help_lists_every_command_once() {
        let cmd = command();
        let grouped: Vec<&str> = GROUPS
            .iter()
            .flat_map(|(_, names)| names.iter().copied())
            .collect();
        for sub in cmd.get_subcommands() {
            let count = grouped.iter().filter(|g| **g == sub.get_name()).count();
            assert_eq!(
                count,
                1,
                "{} must be in exactly one help group",
                sub.get_name()
            );
        }
        let help = command().render_help().to_string();
        for sub in cmd.get_subcommands() {
            assert!(
                help.contains(sub.get_name()),
                "{} missing from help",
                sub.get_name()
            );
        }
        assert!(help.contains("Options:") && help.contains("Examples:"));
    }

    #[test]
    fn long_version_names_the_emulated_clients() {
        let version = command().render_long_version();
        assert!(version.contains(crate::client::emulation::HAPP_VERSION));
    }
}
