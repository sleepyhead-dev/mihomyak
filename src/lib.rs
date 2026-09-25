//! mihomyak: a lightweight mihomo supervisor for CIS-style subscription panels
//! (Remnawave, Marzban, PasarGuard, 3x-ui) that emulates FlClashX / Koala Clash.
//!
//! Architecture overview: `docs/ARCHITECTURE.md`.

pub mod log;

pub mod api;
pub mod cli;
pub mod commands;
pub mod config;
pub mod core;
pub mod emulation;
pub mod http;
pub mod identity;
pub mod profile;
pub mod store;
pub mod subscription;
pub mod supervisor;
#[cfg(feature = "tui")]
pub mod tui;
pub mod updater;
pub mod util;
