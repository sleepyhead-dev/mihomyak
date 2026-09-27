//! mihomyak: a lightweight mihomo supervisor for CIS-style subscription panels
//! (Remnawave, Marzban, PasarGuard, 3x-ui) that emulates FlClashX / Koala Clash / Happ.
//!
//! Architecture overview: `docs/dev/ARCHITECTURE.md`.

pub mod cli;
pub mod client;
pub mod config;
pub mod gateway;
pub mod mihomo;
pub mod service;
pub mod subscription;
pub mod util;
