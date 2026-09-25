//! The emulated device: machine-id seed and os-release facts.
//!
//! Every emulated client derives its headers from the same raw facts a desktop
//! Linux install exposes (`/etc/machine-id`, `/etc/os-release`, kernel release).
//! Client-specific formulas live in `emulation.rs`.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::store::Store;

#[derive(Debug, Clone)]
pub struct Identity {
    /// Raw machine-id (trimmed), the seed for every HWID formula.
    pub machine_id: String,
    pub os: OsRelease,
    /// `uname -r`, Koala's last-resort `x-ver-os`.
    pub kernel_release: String,
}

impl Identity {
    pub fn load(config: &Config, store: &Store) -> Result<Self> {
        let machine_id = match &config.device.machine_id {
            Some(id) => id.trim().to_owned(),
            None => store.machine_id()?,
        };
        if machine_id.is_empty() {
            bail!("device.machine_id is empty");
        }
        let mut os = OsRelease::read(&config.device.os_release);
        if let Some(v) = &config.device.os_name {
            os.set("NAME", v);
        }
        if let Some(v) = &config.device.os_version {
            os.set("VERSION_ID", v);
        }
        if let Some(v) = &config.device.os_pretty_name {
            os.set("PRETTY_NAME", v);
        }
        let kernel_release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|s| s.trim().to_owned())
            .unwrap_or_default();
        Ok(Self {
            machine_id,
            os,
            kernel_release,
        })
    }
}

/// Contents of an os-release file, kept raw because the emulated clients parse it
/// differently (device_info_plus unquotes values, Koala applies ad-hoc regexes).
#[derive(Debug, Clone, Default)]
pub struct OsRelease {
    raw: String,
}

impl OsRelease {
    /// Reads `path`, falling back to `/usr/lib/os-release` like systemd and
    /// device_info_plus do. A missing file yields an empty record.
    pub fn read(path: &Path) -> Self {
        let raw = std::fs::read_to_string(path)
            .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))
            .unwrap_or_default();
        Self { raw }
    }

    pub fn from_raw(raw: impl Into<String>) -> Self {
        Self { raw: raw.into() }
    }

    /// Value per the os-release spec: `KEY=value`, optionally single/double quoted.
    pub fn get(&self, key: &str) -> Option<String> {
        self.raw.lines().rev().find_map(|line| {
            let value = line.trim().strip_prefix(key)?.strip_prefix('=')?.trim();
            let unquoted = ['"', '\'']
                .iter()
                .find_map(|q| value.strip_prefix(*q)?.strip_suffix(*q))
                .unwrap_or(value);
            Some(unquoted.to_owned()).filter(|v| !v.is_empty())
        })
    }

    /// Koala Clash's `/^KEY="?([^"\n]+)"?/m`: first match wins, value stops at a
    /// double quote, single quotes are kept verbatim.
    pub fn koala_get(&self, key: &str) -> Option<String> {
        self.raw.lines().find_map(|line| {
            let value = line.strip_prefix(key)?.strip_prefix('=')?;
            let value = value.strip_prefix('"').unwrap_or(value);
            let end = value.find('"').unwrap_or(value.len());
            Some(value[..end].to_owned()).filter(|v| !v.is_empty())
        })
    }

    /// Replaces every occurrence of one key, so both parsers see the override.
    fn set(&mut self, key: &str, value: &str) {
        let kept: Vec<&str> = self
            .raw
            .lines()
            .filter(|l| !l.starts_with(&format!("{key}=")))
            .collect();
        self.raw = format!("{key}=\"{value}\"\n{}", kept.join("\n"));
    }
}

/// Validates a machine-id supplied by the user or read from disk.
pub fn is_valid_machine_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.chars().all(|c| c.is_ascii_graphic())
}

/// A fresh machine-id in systemd's format (32 lower-case hex digits).
pub fn generate_machine_id() -> Result<String> {
    crate::util::random_hex(16).context("generate machine-id")
}

#[cfg(test)]
mod tests {
    use super::*;

    const UBUNTU: &str = r#"PRETTY_NAME="Ubuntu 24.04.3 LTS"
NAME="Ubuntu"
VERSION_ID="24.04"
VERSION="24.04.3 LTS (Noble Numbat)"
ID=ubuntu
"#;

    #[test]
    fn parses_spec_values() {
        let os = OsRelease::from_raw(UBUNTU);
        assert_eq!(os.get("NAME").as_deref(), Some("Ubuntu"));
        assert_eq!(os.get("VERSION_ID").as_deref(), Some("24.04"));
        assert_eq!(os.get("ID").as_deref(), Some("ubuntu"));
        assert_eq!(os.get("MISSING"), None);
        // NAME must not match PRETTY_NAME.
        assert_eq!(os.koala_get("NAME").as_deref(), Some("Ubuntu"));
        assert_eq!(
            os.koala_get("PRETTY_NAME").as_deref(),
            Some("Ubuntu 24.04.3 LTS")
        );
    }

    #[test]
    fn koala_regex_quirks() {
        let os = OsRelease::from_raw("NAME='Weird'\nVERSION_ID=\n");
        assert_eq!(os.get("NAME").as_deref(), Some("Weird"));
        assert_eq!(os.koala_get("NAME").as_deref(), Some("'Weird'"));
        assert_eq!(os.get("VERSION_ID"), None);
        assert_eq!(os.koala_get("VERSION_ID"), None);
    }

    #[test]
    fn overrides_fields() {
        let mut os = OsRelease::from_raw(UBUNTU);
        os.set("NAME", "Debian GNU/Linux");
        assert_eq!(os.get("NAME").as_deref(), Some("Debian GNU/Linux"));
        assert_eq!(os.koala_get("NAME").as_deref(), Some("Debian GNU/Linux"));
        assert_eq!(os.get("VERSION_ID").as_deref(), Some("24.04"));
    }

    #[test]
    fn machine_ids() {
        let id = generate_machine_id().unwrap();
        assert_eq!(id.len(), 32);
        assert!(is_valid_machine_id(&id));
        assert!(!is_valid_machine_id("has space"));
        assert!(!is_valid_machine_id(""));
    }
}
