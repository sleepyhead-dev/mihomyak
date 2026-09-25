//! The mihomo process: locating, installing, starting and stopping it.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::http::{Client, Endpoint, Request, Url};
use crate::store::Store;

pub struct CoreProcess {
    child: Child,
    started: Instant,
}

impl CoreProcess {
    pub fn spawn(bin: &Path, home: &Path, config: &Config) -> Result<Self> {
        let mut cmd = Command::new(bin);
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
/// start if that download fails.
pub fn seed_geodata(src: &Path, home: &Path) {
    for name in GEODATA_FILES {
        let (from, to) = (src.join(name), home.join(name));
        if from.is_file() && !to.exists() {
            match std::fs::copy(&from, &to) {
                Ok(_) => crate::info!("seeded {name} from {}", src.display()),
                Err(e) => crate::warn!("could not copy {}: {e}", from.display()),
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
    let in_path = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(bin))
            .find(|candidate| candidate.is_file())
    });
    in_path
        .or_else(|| Some(store.root().join("bin").join(bin)).filter(|p| p.is_file()))
        .unwrap_or_else(|| bin.clone())
}

pub fn version(bin: &Path) -> Result<String> {
    let out = Command::new(bin)
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

/// Downloads a mihomo release (`version` or latest) to `dest`.
pub fn install(version: Option<&str>, dest: &Path, mirror: Option<&str>) -> Result<String> {
    let base = mirror.unwrap_or("https://github.com").trim_end_matches('/');
    let client = Client {
        max_body: 256 * 1024 * 1024,
        io_timeout: Duration::from_secs(120),
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
    let mut binary = Vec::new();
    flate2::read::GzDecoder::new(&response.body[..])
        .read_to_end(&mut binary)
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
        bail!("unexpected release redirect {location:?}");
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
                url = url.join(location)?;
            }
            _ => return Ok(response),
        }
    }
    bail!("too many redirects")
}
