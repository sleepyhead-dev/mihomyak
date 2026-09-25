//! Command-line interface.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::emulation::ClientKind;

#[derive(Debug, Parser)]
#[command(
    name = "mihomyak",
    version,
    about = "Lightweight mihomo supervisor for CIS subscriptions"
)]
pub struct Cli {
    /// Config file (default: $MIHOMYAK_CONFIG, then ~/.config/mihomyak/config.toml
    /// or /etc/mihomyak/config.toml for root).
    #[arg(short, long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Data directory (default: $MIHOMYAK_DATA_DIR, ~/.local/share/mihomyak,
    /// /var/lib/mihomyak for root).
    #[arg(short, long, global = true, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run and supervise mihomo in the foreground, updating the subscription on schedule.
    Run,
    /// Update the subscription now (signals a running supervisor, else applies directly).
    Update,
    /// Diagnose the subscription: fetch and analyse it without applying anything.
    Fetch {
        /// Impersonate another client for this request.
        #[arg(long, value_name = "CLIENT", value_parser = parse_client)]
        client: Option<ClientKind>,
        /// Override the User-Agent for this request.
        #[arg(long, value_name = "UA")]
        user_agent: Option<String>,
        /// Print the response body.
        #[arg(long)]
        body: bool,
    },
    /// Show the emulated device and the exact request headers.
    Identity {
        /// Show what another client would send.
        #[arg(long, value_name = "CLIENT", value_parser = parse_client)]
        client: Option<ClientKind>,
    },
    /// Subscription usage and core state.
    Status,
    /// List proxy groups, or the proxies of one group.
    #[command(alias = "groups")]
    Proxies {
        /// Group name: exact, case-insensitive or a unique part of it.
        group: Option<String>,
    },
    /// Select a proxy in a group.
    Select {
        /// Group name: exact, case-insensitive or a unique part of it.
        group: String,
        /// Proxy name, matched the same way.
        proxy: String,
    },
    /// Measure proxy delays (all selectable groups, or one group).
    Test {
        /// Group name: exact, case-insensitive or a unique part of it.
        group: Option<String>,
        /// URL requested through each proxy.
        #[arg(long, default_value = crate::profile::HEALTH_CHECK_URL)]
        url: String,
        /// Per-proxy timeout.
        #[arg(long, default_value_t = crate::api::DELAY_TIMEOUT_MS, value_name = "MS")]
        timeout: u32,
    },
    /// Show or set the routing mode.
    Mode {
        /// New mode (omit to show the current one).
        #[arg(value_parser = ["rule", "global", "direct"])]
        mode: Option<String>,
    },
    /// Interactive terminal UI.
    #[cfg(feature = "tui")]
    Tui,
    /// Print the mihomo config that would be generated from the cached subscription.
    Render,
    /// Validate the settings and the generated config (runs `mihomo -t`).
    Check,
    /// Exit 0 if the core API answers (for container health checks).
    Health,
    /// Manage the mihomo binary.
    #[command(subcommand)]
    Core(CoreCommand),
}

#[derive(Debug, Subcommand)]
pub enum CoreCommand {
    /// Download mihomo from GitHub releases for this CPU.
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

fn parse_client(s: &str) -> Result<ClientKind, String> {
    s.parse().map_err(|e: anyhow::Error| e.to_string())
}
