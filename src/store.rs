//! Persistent state in the data directory.
//!
//! ```text
//! <data>/
//!   machine-id               generated HWID seed (never changes once created)
//!   secret                   generated mihomo API secret
//!   mode                     routing mode chosen at runtime (rule/global/direct)
//!   supervisor.pid           pid of the running `mihomyak run`
//!   subscription/body        last applied (known good) response body
//!   subscription/meta.json   metadata of the last fetch attempt
//!   mihomo/                  mihomo home: config.yaml, providers/, cache.db, geodata
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::identity::{generate_machine_id, is_valid_machine_id};
use crate::util::write_atomic;

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

/// What we remember about the subscription between runs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubscriptionMeta {
    /// Unix time the stored body was fetched.
    pub fetched_at: u64,
    /// Unix time of the last attempt (successful or not).
    pub checked_at: u64,
    /// Why the last attempt did not replace the stored body.
    pub last_error: Option<String>,
    /// Response headers of the stored body (provider info is derived from these).
    pub headers: Vec<(String, String)>,
    /// Cache key `<client>:<sha256(url)[..16]>`: the body is reused only for the
    /// same subscription URL and emulated client.
    pub client: String,
    /// `mihomo` or `links`.
    pub format: String,
    /// Number of proxies found in the stored body.
    pub proxies: usize,
    /// FlClashX `flclashx-newdomain` redirect: (configured URL, URL to use instead).
    pub url_override: Option<(String, String)>,
}

impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root.join("subscription"))
            .and_then(|()| fs::create_dir_all(root.join("mihomo")))
            .with_context(|| format!("create data directory {}", root.display()))?;
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn mihomo_home(&self) -> PathBuf {
        self.root.join("mihomo")
    }

    pub fn mihomo_config(&self) -> PathBuf {
        self.mihomo_home().join("config.yaml")
    }

    pub fn pid_file(&self) -> PathBuf {
        self.root.join("supervisor.pid")
    }

    /// The persisted machine-id, created on first use.
    pub fn machine_id(&self) -> Result<String> {
        self.load_or_create("machine-id", generate_machine_id, is_valid_machine_id)
    }

    /// The persisted API secret, created on first use.
    pub fn secret(&self) -> Result<String> {
        self.load_or_create("secret", || crate::util::random_hex(16), |s| !s.is_empty())
    }

    fn load_or_create(
        &self,
        name: &str,
        create: impl FnOnce() -> Result<String>,
        valid: impl Fn(&str) -> bool,
    ) -> Result<String> {
        let path = self.root.join(name);
        match fs::read_to_string(&path) {
            Ok(value) => {
                let value = value.trim().to_owned();
                if !valid(&value) {
                    bail!("{} is corrupt; fix or delete it", path.display());
                }
                Ok(value)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let value = create()?;
                write_atomic(&path, format!("{value}\n").as_bytes())?;
                restrict_permissions(&path);
                Ok(value)
            }
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    pub fn load_meta(&self) -> Option<SubscriptionMeta> {
        let text = fs::read(self.root.join("subscription/meta.json")).ok()?;
        serde_json::from_slice(&text)
            .inspect_err(|e| crate::warn!("ignoring unreadable subscription/meta.json: {e}"))
            .ok()
    }

    pub fn save_meta(&self, meta: &SubscriptionMeta) -> Result<()> {
        let json = serde_json::to_vec_pretty(meta)?;
        write_atomic(&self.root.join("subscription/meta.json"), &json)
    }

    pub fn load_body(&self) -> Option<Vec<u8>> {
        fs::read(self.root.join("subscription/body")).ok()
    }

    pub fn save_body(&self, body: &[u8]) -> Result<()> {
        let path = self.root.join("subscription/body");
        write_atomic(&path, body)?;
        restrict_permissions(&path);
        Ok(())
    }

    /// Records the running supervisor.
    pub fn write_pid(&self) -> Result<()> {
        write_atomic(
            &self.pid_file(),
            format!("{}\n", std::process::id()).as_bytes(),
        )
    }

    /// Routing mode chosen at runtime (`mihomyak mode`, TUI), if any.
    pub fn mode(&self) -> Option<String> {
        let mode = fs::read_to_string(self.root.join("mode")).ok()?;
        Some(mode.trim().to_owned()).filter(|m| crate::config::is_mode(m))
    }

    pub fn set_mode(&self, mode: &str) -> Result<()> {
        write_atomic(&self.root.join("mode"), format!("{mode}\n").as_bytes())
    }

    pub fn remove_pid(&self) {
        let _ = fs::remove_file(self.pid_file());
    }

    /// Pid of a live `mihomyak run` using this data directory, if any.
    pub fn supervisor_pid(&self) -> Option<i32> {
        let pid: i32 = fs::read_to_string(self.pid_file())
            .ok()?
            .trim()
            .parse()
            .ok()?;
        // SAFETY: signal 0 only checks that the process exists.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        (alive && pid != std::process::id() as i32).then_some(pid)
    }
}

/// Secrets and the subscription body (it embeds proxy credentials) are owner-only.
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_generated_values() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let id = store.machine_id().unwrap();
        assert_eq!(store.machine_id().unwrap(), id);
        assert_ne!(store.secret().unwrap(), id);

        fs::write(dir.path().join("machine-id"), "bad id\n").unwrap();
        assert!(store.machine_id().is_err());
    }

    #[test]
    fn round_trips_subscription() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        assert!(store.load_meta().is_none());
        let meta = SubscriptionMeta {
            fetched_at: 1,
            headers: vec![("profile-title".into(), "x".into())],
            ..Default::default()
        };
        store.save_meta(&meta).unwrap();
        store.save_body(b"proxies: []").unwrap();
        assert_eq!(store.load_meta().unwrap().headers, meta.headers);
        assert_eq!(store.load_body().unwrap(), b"proxies: []");
    }

    #[test]
    fn detects_supervisor() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.supervisor_pid(), None);
        // Our own pid is never reported as "another" supervisor.
        store.write_pid().unwrap();
        assert_eq!(store.supervisor_pid(), None);
    }
}
