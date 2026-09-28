//! The emulated device: machine-id seed and os-release facts.
//!
//! Every emulated client derives its headers from the same raw facts a desktop
//! Linux install exposes (`/etc/machine-id`, `/etc/os-release`, kernel release).
//! Client-specific formulas live in `client/emulation.rs`.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::service::store::Store;

#[derive(Debug, Clone)]
pub struct Identity {
    /// Raw machine-id (trimmed), the seed for every HWID formula.
    pub machine_id: String,
    pub os: OsRelease,
    /// `uname -r`, Koala's last-resort `x-ver-os`.
    pub kernel_release: String,
    /// Kernel hostname (Happ's `X-Device-Model`).
    pub hostname: String,
    /// UI locale, e.g. `en` or `ru_RU` (Happ's `X-Device-Locale`).
    pub locale: String,
}

impl Identity {
    pub fn load(config: &Config, store: &Store) -> Result<Self> {
        let machine_id = match (&config.device.machine_id, &config.device.seed) {
            (Some(id), _) => id.trim().to_owned(),
            (None, Some(seed)) => machine_id_from_seed(seed),
            (None, None) => store.machine_id()?,
        };
        if machine_id.is_empty() {
            bail!("device.machine_id is empty");
        }
        let os = OsRelease::read(&config.device.os_release);
        let kernel_release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|s| s.trim().to_owned())
            .unwrap_or_default();
        let hostname = config.device.hostname.clone().unwrap_or_else(|| {
            std::fs::read_to_string("/proc/sys/kernel/hostname")
                .map(|s| s.trim().to_owned())
                .unwrap_or_else(|_| "localhost".into())
        });
        Ok(Self {
            machine_id,
            os,
            kernel_release,
            hostname,
            locale: config.device.locale.clone(),
        })
    }
}

/// os-release facts, kept raw because every emulated client parses them its own
/// way (see `docs/dev/SUBSCRIPTIONS.md` §7): device_info_plus (FlClashX), Koala's
/// regexes, Qt's QSysInfo (Happ).
#[derive(Debug, Clone, Default)]
pub struct OsRelease {
    /// The configured os-release file (normally `/etc/os-release`).
    primary: Option<String>,
    /// `/usr/lib/os-release`, used when the primary file is missing.
    fallback: Option<String>,
    /// `/etc/lsb-release` (device_info_plus falls back to its `DISTRIB_*` keys).
    lsb: Option<String>,
}

impl OsRelease {
    pub fn read(path: &Path) -> Self {
        let read = |p: &str| std::fs::read_to_string(p).ok();
        Self {
            primary: std::fs::read_to_string(path).ok(),
            fallback: read("/usr/lib/os-release"),
            lsb: read("/etc/lsb-release"),
        }
    }

    pub fn from_raw(raw: impl Into<String>) -> Self {
        Self {
            primary: Some(raw.into()),
            ..Self::default()
        }
    }

    pub fn with_lsb(mut self, raw: impl Into<String>) -> Self {
        self.lsb = Some(raw.into());
        self
    }

    fn effective(&self) -> Option<&str> {
        self.primary.as_deref().or(self.fallback.as_deref())
    }

    /// Value per the os-release spec (systemd, Qt): `KEY=value`, optionally
    /// single/double quoted; the last assignment wins.
    pub fn get(&self, key: &str) -> Option<String> {
        self.effective()?.lines().rev().find_map(|line| {
            let value = line.trim().strip_prefix(key)?.strip_prefix('=')?.trim();
            let unquoted = ['"', '\'']
                .iter()
                .find_map(|q| value.strip_prefix(*q)?.strip_suffix(*q))
                .unwrap_or(value);
            Some(unquoted.to_owned()).filter(|v| !v.is_empty())
        })
    }

    /// device_info_plus `toKeyValues()`: a line is split on every `=`; unless that
    /// yields exactly two parts the value is null. Only double quotes are removed
    /// (prefix and suffix independently), nothing is trimmed, the last line wins.
    /// An empty value stays `Some("")`.
    pub fn dip_get(&self, key: &str) -> Option<String> {
        dip_lookup(self.effective()?, key)
    }

    /// device_info_plus lookup in `/etc/lsb-release`.
    pub fn dip_lsb(&self, key: &str) -> Option<String> {
        dip_lookup(self.lsb.as_deref()?, key)
    }

    /// Koala Clash's `/^KEY="?([^"\n]+)"?/m` over `cat /etc/os-release` (no
    /// `/usr/lib` fallback): first match wins, the value stops at a double quote,
    /// single quotes are kept verbatim.
    pub fn koala_get(&self, key: &str) -> Option<String> {
        self.primary.as_deref()?.lines().find_map(|line| {
            let value = line.strip_prefix(key)?.strip_prefix('=')?;
            let value = value.strip_prefix('"').unwrap_or(value);
            let end = value.find('"').unwrap_or(value.len());
            Some(value[..end].to_owned()).filter(|v| !v.is_empty())
        })
    }
}

fn dip_lookup(raw: &str, key: &str) -> Option<String> {
    let mut found = None;
    for line in raw.lines() {
        let parts: Vec<&str> = line.split('=').collect();
        if parts.len() == 2 && parts[0] == key {
            let value = parts[1].strip_prefix('"').unwrap_or(parts[1]);
            let value = value.strip_suffix('"').unwrap_or(value);
            found = Some(value.to_owned());
        } else if parts.len() != 2 && line == key {
            found = None;
        }
    }
    found
}

/// Validates a machine-id supplied by the user or read from disk.
pub fn is_valid_machine_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.chars().all(|c| c.is_ascii_graphic())
}

/// A fresh machine-id in systemd's format (32 lower-case hex digits).
pub fn generate_machine_id() -> Result<String> {
    crate::util::random_hex(16).context("generate machine-id")
}

/// The machine-id a `device.seed` stands for, in systemd's format. The prefix
/// keeps it unrelated to any other use of the same string.
pub fn machine_id_from_seed(seed: &str) -> String {
    let input = format!("mihomyak device seed\0{}", seed.trim());
    crate::util::sha256_hex(input.as_bytes())[..32].to_owned()
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
    fn parser_quirks() {
        let os = OsRelease::from_raw("NAME='Weird'\nVERSION_ID=\nBUG=a=b\n");
        assert_eq!(os.get("NAME").as_deref(), Some("Weird"));
        assert_eq!(os.koala_get("NAME").as_deref(), Some("'Weird'"));
        assert_eq!(
            os.dip_get("NAME").as_deref(),
            Some("'Weird'"),
            "only double quotes"
        );
        assert_eq!(os.get("VERSION_ID"), None);
        assert_eq!(os.koala_get("VERSION_ID"), None);
        assert_eq!(
            os.dip_get("VERSION_ID").as_deref(),
            Some(""),
            "FlClashX sends it empty"
        );
        assert_eq!(os.dip_get("BUG"), None, "more than one '=' yields null");
    }

    #[test]
    fn lsb_and_fallback_sources() {
        let os = OsRelease::from_raw("NAME=\"Arch Linux\"\n").with_lsb("DISTRIB_RELEASE=rolling\n");
        assert_eq!(os.dip_get("VERSION_ID"), None);
        assert_eq!(os.dip_lsb("DISTRIB_RELEASE").as_deref(), Some("rolling"));
        let only_fallback = OsRelease {
            fallback: Some("NAME=Fallback\n".into()),
            ..OsRelease::default()
        };
        assert_eq!(only_fallback.get("NAME").as_deref(), Some("Fallback"));
        assert_eq!(only_fallback.dip_get("NAME").as_deref(), Some("Fallback"));
        assert_eq!(
            only_fallback.koala_get("NAME"),
            None,
            "Koala reads /etc only"
        );
    }

    #[test]
    fn machine_ids() {
        let id = generate_machine_id().unwrap();
        assert_eq!(id.len(), 32);
        assert!(is_valid_machine_id(&id));
        assert!(!is_valid_machine_id("has space"));
        assert!(!is_valid_machine_id(""));
    }

    #[test]
    fn seeds_give_stable_distinct_machine_ids() {
        let a = machine_id_from_seed("my home server");
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
        assert_eq!(a, machine_id_from_seed("  my home server\n"));
        assert_ne!(a, machine_id_from_seed("my home server 2"));
        // Pinned: changing the derivation would move every seeded user to a new device.
        assert_eq!(
            machine_id_from_seed("seed"),
            &crate::util::sha256_hex(b"mihomyak device seed\0seed")[..32]
        );
    }

    #[test]
    fn seed_or_explicit_machine_id_wins_over_the_stored_one() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut config = Config::default();
        config.device.seed = Some("seed".into());
        let seeded = Identity::load(&config, &store).unwrap().machine_id;
        assert_eq!(seeded, machine_id_from_seed("seed"));
        config.device.seed = None;
        config.device.machine_id = Some("0d0af05ee8fd4dc29275718f2ce4dff1".into());
        let explicit = Identity::load(&config, &store).unwrap().machine_id;
        assert_eq!(explicit, "0d0af05ee8fd4dc29275718f2ce4dff1");
        config.device.machine_id = None;
        let stored = Identity::load(&config, &store).unwrap().machine_id;
        assert_ne!(stored, seeded);
        assert_eq!(stored, store.machine_id().unwrap());
    }
}
