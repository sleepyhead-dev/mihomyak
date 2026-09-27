//! The mihomo process: locating, installing, starting and stopping it.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::http::{Client, Endpoint, Request, Scheme, Url};
use crate::store::Store;

pub struct CoreProcess {
    child: Child,
    started: Instant,
}

/// Environment passed through to mihomo. Everything else (the subscription URL,
/// secrets in `MIHOMYAK_*`, proxies set for mihomyak itself) is withheld.
const CORE_ENV: &[&str] = &[
    "PATH",
    "TZ",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "GOMEMLIMIT",
    "GOGC",
    "SAFE_PATHS",
];

/// A `Command` for the mihomo binary with a scrubbed environment that dies with us:
/// `PR_SET_PDEATHSIG` stops an orphaned core from keeping ports and TUN routes if
/// the supervisor is SIGKILLed.
pub fn command(bin: &Path) -> Command {
    let mut cmd = Command::new(bin);
    cmd.env_clear().envs(
        CORE_ENV
            .iter()
            .filter_map(|k| Some((k, std::env::var_os(k)?))),
    );
    let parent = std::process::id() as libc::pid_t;
    // SAFETY: only async-signal-safe calls (prctl, getppid, _exit) between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            // The parent may have died before prctl took effect.
            if libc::getppid() != parent {
                libc::_exit(1);
            }
            Ok(())
        });
    }
    cmd
}

impl CoreProcess {
    pub fn spawn(bin: &Path, home: &Path, config: &Config) -> Result<Self> {
        let mut cmd = command(bin);
        cmd.arg("-d")
            .arg(home)
            .arg("-f")
            .arg(home.join("config.yaml"))
            .stdin(Stdio::null());
        if let Some(limit) = &config.core.memory_limit {
            // Go runtime soft limit: GC works harder instead of growing the heap.
            cmd.env("GOMEMLIMIT", limit);
        }
        let child = cmd
            .spawn()
            .with_context(|| format!("start {} (install it or set core.bin)", bin.display()))?;
        crate::info!("mihomo started (pid {})", child.id());
        Ok(Self {
            child,
            started: Instant::now(),
        })
    }

    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    /// `Some(status)` once the process has exited.
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        match self.child.try_wait() {
            Ok(status) => Ok(status),
            // Reaped elsewhere (PID 1 orphan reaping raced us): it is gone.
            Err(e) if e.raw_os_error() == Some(libc::ECHILD) => Ok(Some(ExitStatus::default())),
            Err(e) => Err(e.into()),
        }
    }

    /// SIGTERM first so mihomo can remove TUN routes and nftables rules, then SIGKILL.
    pub fn stop(mut self, grace: Duration) {
        if matches!(self.try_wait(), Ok(Some(_))) {
            return;
        }
        let pid = self.child.id() as libc::pid_t;
        // SAFETY: plain kill(2) on our own child's pid, which is not yet reaped.
        unsafe { libc::kill(pid, libc::SIGTERM) };
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if matches!(self.try_wait(), Ok(Some(_))) {
                crate::info!("mihomo stopped");
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        crate::warn!("mihomo ignored SIGTERM for {grace:?}, killing it");
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Geodata file names mihomo looks for in its home directory.
const GEODATA_FILES: &[&str] = &[
    "geoip.metadb",
    "geosite.dat",
    "geoip.dat",
    "GeoLite2-ASN.mmdb",
    "ASN.mmdb",
];

/// Copies bundled geodata into mihomo's home when absent. mihomo would otherwise
/// download it from github.com on first use of a GEOIP/GEOSITE rule and refuse to
/// start if that download fails. Each file is copied to a temporary name and
/// renamed, so an interrupted copy never leaves a truncated database behind.
pub fn seed_geodata(src: &Path, home: &Path) {
    for name in GEODATA_FILES {
        let (from, to) = (src.join(name), home.join(name));
        if !from.is_file() || to.exists() {
            continue;
        }
        let tmp = home.join(format!(".{name}.tmp"));
        match std::fs::copy(&from, &tmp).and_then(|_| std::fs::rename(&tmp, &to)) {
            Ok(()) => crate::info!("seeded {name} from {}", src.display()),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                crate::warn!("could not copy {}: {e}", from.display());
            }
        }
    }
}

/// `core.bin` as a path, else found in PATH, else `<data>/bin/mihomo`.
pub fn resolve_bin(config: &Config, store: &Store) -> PathBuf {
    let bin = &config.core.bin;
    if bin.components().count() > 1 {
        return bin.clone();
    }
    which(bin)
        .or_else(|| Some(store.root().join("bin").join(bin)).filter(|p| p.is_file()))
        .unwrap_or_else(|| bin.clone())
}

/// First regular file called `name` in `PATH`.
pub fn which(name: &Path) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

pub fn version(bin: &Path) -> Result<String> {
    let out = command(bin)
        .arg("-v")
        .output()
        .with_context(|| format!("run {} -v", bin.display()))?;
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text.lines().next().unwrap_or_default().trim().to_owned())
}

/// mihomo release asset suffix for this CPU.
fn release_arch() -> Result<&'static str> {
    Ok(match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "arm" => "armv7",
        "x86" => "386",
        "riscv64" => "riscv64",
        "loongarch64" => "loong64-abi2",
        "s390x" => "s390x",
        other => bail!("no mihomo release for CPU architecture {other}"),
    })
}

/// Largest unpacked mihomo binary accepted (current releases are ~35 MB).
const MAX_BINARY_BYTES: usize = 256 * 1024 * 1024;

/// Downloads a mihomo release (`version` or latest) to `dest`. Downloads from a
/// mirror must be pinned with `sha256`: a mirror is a third party.
pub fn install(
    version: Option<&str>,
    dest: &Path,
    mirror: Option<&str>,
    sha256: Option<&str>,
) -> Result<String> {
    if mirror.is_some() && sha256.is_none() {
        bail!("a GitHub mirror is set: pass --sha256 so the downloaded binary can be verified");
    }
    let base = mirror.unwrap_or("https://github.com").trim_end_matches('/');
    let client = Client {
        max_body: 128 * 1024 * 1024,
        io_timeout: Duration::from_secs(60),
        total_timeout: Duration::from_secs(30 * 60),
        ..Client::default()
    };
    let tag = match version {
        Some(v) if v.starts_with('v') => v.to_owned(),
        Some(v) => format!("v{v}"),
        None => latest_tag(&client, base)?,
    };
    let arch = release_arch()?;
    let url =
        format!("{base}/MetaCubeX/mihomo/releases/download/{tag}/mihomo-linux-{arch}-{tag}.gz");
    crate::info!("downloading {url}");
    let response = get(&client, &Url::parse(&url)?, 5)?;
    if response.status != 200 {
        bail!(
            "download failed: HTTP {} {}",
            response.status,
            response.reason
        );
    }
    if let Some(expected) = sha256 {
        let actual = crate::util::sha256_hex(&response.body);
        if !actual.eq_ignore_ascii_case(expected.trim()) {
            bail!("checksum mismatch: expected {expected}, got {actual}");
        }
        crate::info!("sha256 verified");
    }
    let binary = crate::http::inflate(
        flate2::read::GzDecoder::new(&response.body[..]),
        MAX_BINARY_BYTES,
    )
    .context("unpack mihomo archive")?;
    crate::util::write_atomic(dest, &binary)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))?;
    Ok(tag)
}

/// Reads the tag from the `/releases/latest` redirect (no API token needed).
fn latest_tag(client: &Client, base: &str) -> Result<String> {
    let url = Url::parse(&format!("{base}/MetaCubeX/mihomo/releases/latest"))?;
    let response = get(client, &url, 0)?;
    let location = response
        .header("location")
        .context("could not resolve the latest mihomo release; pass --version")?;
    let tag = location.rsplit('/').next().unwrap_or_default();
    if !tag.starts_with('v') {
        bail!("unexpected release redirect");
    }
    Ok(tag.to_owned())
}

fn get(client: &Client, url: &Url, max_redirects: usize) -> Result<crate::http::Response> {
    let mut url = url.clone();
    for _ in 0..=max_redirects {
        let headers = vec![
            ("Host".to_owned(), url.host_header()),
            (
                "User-Agent".into(),
                format!("mihomyak/{}", env!("CARGO_PKG_VERSION")),
            ),
            ("Accept".into(), "*/*".into()),
            ("Connection".into(), "close".into()),
        ];
        let request = Request {
            method: "GET",
            target: &url.target,
            headers: &headers,
            body: &[],
        };
        let response = client.send(&Endpoint::from(&url), &request)?;
        match response.header("location") {
            Some(location) if (300..400).contains(&response.status) && max_redirects > 0 => {
                let next = url.join(location)?;
                if url.scheme == Scheme::Https && next.scheme == Scheme::Http {
                    bail!("refusing a redirect from https to http");
                }
                url = next;
            }
            _ => return Ok(response),
        }
    }
    bail!("too many redirects")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_bin_prefers_an_explicit_path() {
        let dir = tempfile::tempdir().unwrap();
        let bin_path = dir.path().join("custom-mihomo");
        std::fs::write(&bin_path, b"#!/bin/sh\n").unwrap();
        let mut config = Config::default();
        config.core.bin = bin_path.clone();
        let store = Store::open(&dir.path().join("data")).unwrap();
        assert_eq!(resolve_bin(&config, &store), bin_path);
    }

    #[test]
    fn resolve_bin_falls_back_to_the_data_dir_when_missing_from_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("data")).unwrap();
        // Distinctive name: it must not exist anywhere on the real PATH.
        let name = "mihomyak-test-fake-core-bin";
        let bin_dir = store.root().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let seeded = bin_dir.join(name);
        std::fs::write(&seeded, b"#!/bin/sh\n").unwrap();

        let mut config = Config::default();
        config.core.bin = PathBuf::from(name);
        assert_eq!(resolve_bin(&config, &store), seeded);
    }

    #[test]
    fn resolve_bin_falls_back_to_the_bare_name_when_nowhere_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("data")).unwrap();
        let name = "mihomyak-test-missing-core-bin";
        let mut config = Config::default();
        config.core.bin = PathBuf::from(name);
        assert_eq!(resolve_bin(&config, &store), PathBuf::from(name));
    }

    #[test]
    fn seed_geodata_copies_missing_files_but_never_overwrites() {
        let src_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        std::fs::write(src_dir.path().join("geoip.metadb"), b"new-geoip").unwrap();
        std::fs::write(src_dir.path().join("geosite.dat"), b"new-geosite").unwrap();
        // geoip.dat / GeoLite2-ASN.mmdb / ASN.mmdb are intentionally absent from src.
        std::fs::write(home_dir.path().join("geosite.dat"), b"existing-geosite").unwrap();

        seed_geodata(src_dir.path(), home_dir.path());

        assert_eq!(
            std::fs::read(home_dir.path().join("geoip.metadb")).unwrap(),
            b"new-geoip"
        );
        assert_eq!(
            std::fs::read(home_dir.path().join("geosite.dat")).unwrap(),
            b"existing-geosite",
            "an existing file is never overwritten"
        );
        assert!(!home_dir.path().join("geoip.dat").exists());
    }
}
