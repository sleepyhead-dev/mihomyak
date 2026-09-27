//! Persistent state in the data directory.
//!
//! ```text
//! <data>/
//!   machine-id               generated HWID seed (never changes once created)
//!   secret                   generated mihomo API secret
//!   mode                     routing mode chosen at runtime (rule/global/direct)
//!   supervisor.pid           pid of the running `mihomyak run` (flock-held while it runs)
//!   subscription/body        last applied (known good) response body
//!   subscription/meta.json   metadata of the last fetch attempt
//!   mihomo/                  mihomo home: config.yaml, providers/, cache.db, geodata
//! ```

use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::identity::{generate_machine_id, is_valid_machine_id};
use crate::util::{write_atomic, write_new};

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
    /// Body format: `mihomo`, `links` or `xray-json` (empty forces a rebuild).
    pub format: String,
    /// Number of proxies found in the stored body.
    pub proxies: usize,
    /// FlClashX `flclashx-newdomain` redirect: (configured URL, URL to use instead).
    pub url_override: Option<(String, String)>,
    /// When the running supervisor plans the next update (unix time).
    #[serde(default)]
    pub next_update_at: Option<u64>,
    /// Incremented by every update attempt (`mihomyak update` waits for it).
    #[serde(default)]
    pub update_seq: u64,
    /// Hosts the last successful fetch talked to (URL, redirects).
    #[serde(default)]
    pub panel_hosts: Vec<String>,
    /// Addresses those hosts resolved to (gateway mode keeps them off the tunnel).
    #[serde(default)]
    pub panel_ips: Vec<std::net::IpAddr>,
}

impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let existed = root.exists();
        fs::create_dir_all(root.join("subscription"))
            .and_then(|()| fs::create_dir_all(root.join("mihomo")))
            .with_context(|| format!("create data directory {}", root.display()))?;
        // Holds the subscription (credentials), API secret and HWID seed. A directory
        // the user pointed us at is not chmod-ed behind their back, only reported.
        if !existed {
            if let Err(e) = fs::set_permissions(root, fs::Permissions::from_mode(0o700)) {
                crate::debug!("cannot restrict {}: {e}", root.display());
            }
        } else if fs::metadata(root).is_ok_and(|m| m.permissions().mode() & 0o077 != 0) {
            crate::debug!(
                "data directory {} is accessible to other users; files inside stay 0600",
                root.display()
            );
        }
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
                if write_new(&path, format!("{value}\n").as_bytes())? {
                    restrict_permissions(&path);
                    return Ok(value);
                }
                // Another process created it first: everyone must use its value.
                let value = fs::read_to_string(&path)?.trim().to_owned();
                if !valid(&value) {
                    bail!("{} is corrupt; fix or delete it", path.display());
                }
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

    pub fn body_path(&self) -> PathBuf {
        self.root.join("subscription/body")
    }

    pub fn load_body(&self) -> Option<Vec<u8>> {
        fs::read(self.body_path()).ok()
    }

    pub fn save_body(&self, body: &[u8]) -> Result<()> {
        let path = self.body_path();
        write_atomic(&path, body)?;
        restrict_permissions(&path);
        Ok(())
    }

    /// Takes the supervisor lock for this data directory and records our pid.
    /// The lock (an `flock` on the pid file) is released when the process exits,
    /// however it exits, so a stale pid file never blocks a restart.
    pub fn lock_supervisor(&self) -> Result<SupervisorLock> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let path = self.pid_file();
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        if !flock(&file, libc::LOCK_EX | libc::LOCK_NB) {
            let pid = fs::read_to_string(&path).unwrap_or_default();
            bail!(
                "another supervisor (pid {}) already uses {}",
                pid.trim(),
                self.root.display()
            );
        }
        file.set_len(0)?;
        writeln!(file, "{}", std::process::id())?;
        Ok(SupervisorLock { file })
    }

    /// Routing mode chosen at runtime (`mihomyak mode`, TUI), if any.
    pub fn mode(&self) -> Option<String> {
        let mode = fs::read_to_string(self.root.join("mode")).ok()?;
        Some(mode.trim().to_owned()).filter(|m| crate::config::is_mode(m))
    }

    pub fn set_mode(&self, mode: &str) -> Result<()> {
        write_atomic(&self.root.join("mode"), format!("{mode}\n").as_bytes())
    }

    /// Pid of a live `mihomyak run` using this data directory, if any. Liveness is
    /// the lock, not the pid: a recycled pid of an unrelated process never counts.
    pub fn supervisor_pid(&self) -> Option<i32> {
        let file = fs::File::open(self.pid_file()).ok()?;
        if flock(&file, libc::LOCK_SH | libc::LOCK_NB) {
            // Nobody holds the lock: no supervisor.
            flock(&file, libc::LOCK_UN);
            return None;
        }
        let pid: i32 = std::io::read_to_string(&file).ok()?.trim().parse().ok()?;
        (pid > 0 && pid != std::process::id() as i32).then_some(pid)
    }

    /// Sends `signal` to the running supervisor. `Ok(None)` if none is running.
    pub fn signal_supervisor(&self, signal: libc::c_int) -> Result<Option<i32>> {
        let Some(pid) = self.supervisor_pid() else {
            return Ok(None);
        };
        // SAFETY: kill(2) with a positive pid of a process holding our lock.
        if unsafe { libc::kill(pid, signal) } != 0 {
            bail!(
                "cannot signal the supervisor (pid {pid}): {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(Some(pid))
    }
}

/// Held by the running supervisor; dropping it releases the lock.
pub struct SupervisorLock {
    file: fs::File,
}

impl Drop for SupervisorLock {
    fn drop(&mut self) {
        // Clear the pid but keep the file: unlinking a lock file races with a new
        // supervisor that already opened it.
        let _ = self.file.set_len(0);
    }
}

/// Non-blocking `flock(2)`; `true` if the lock was acquired.
fn flock(file: &fs::File, operation: libc::c_int) -> bool {
    // SAFETY: flock on a file descriptor we own for the duration of the call.
    unsafe { libc::flock(file.as_raw_fd(), operation) == 0 }
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
    fn restricts_permissions_on_the_data_dir_and_secret_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("data");
        let store = Store::open(&root).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&root), 0o700);

        store.machine_id().unwrap();
        assert_eq!(mode(&root.join("machine-id")), 0o600);

        store.secret().unwrap();
        assert_eq!(mode(&root.join("secret")), 0o600);

        store.save_body(b"proxies: []").unwrap();
        assert_eq!(mode(&store.body_path()), 0o600);
    }

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
    fn detects_supervisor_by_lock() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.supervisor_pid(), None);
        // A stale pid file without a lock holder means nothing is running.
        fs::write(store.pid_file(), "1\n").unwrap();
        assert_eq!(store.supervisor_pid(), None);
        assert_eq!(store.signal_supervisor(0).unwrap(), None);

        let lock = store.lock_supervisor().unwrap();
        // flock locks belong to the open file description, so a second open in the
        // same process conflicts just like another process would.
        assert!(store.lock_supervisor().is_err());
        // Our own pid is never reported as "another" supervisor.
        assert_eq!(store.supervisor_pid(), None);
        fs::write(store.pid_file(), "0\n").unwrap();
        assert_eq!(
            store.supervisor_pid(),
            None,
            "pid 0 must never be signalled"
        );
        drop(lock);
        assert!(store.lock_supervisor().is_ok());
    }

    #[test]
    fn concurrent_creation_agrees_on_one_value() {
        let dir = tempfile::tempdir().unwrap();
        let ids: Vec<String> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|_| s.spawn(|| Store::open(dir.path()).unwrap().machine_id().unwrap()))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(ids.windows(2).all(|w| w[0] == w[1]));
    }
}
