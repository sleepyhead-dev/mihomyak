//! Subscription update pipeline shared by the supervisor and one-shot commands:
//! fetch → analyse → build → validate with `mihomo -t` → back up → write → record.
//!
//! A response only replaces the running config when every step succeeds; the
//! previous files are kept as `*.prev` so a config the live core still rejects
//! can be rolled back ([`Updater::rollback`]). The updater never touches the
//! mihomo process itself; callers decide whether to reload.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::config::{Config, Interval};
use crate::emulation::{ClientKind, Emulation};
use crate::http::{self, Response, Scheme, Url};
use crate::identity::Identity;
use crate::profile::{Built, Params};
use crate::store::{Store, SubscriptionMeta};
use crate::subscription::{self, Analysis, Problem, ProviderInfo};
use crate::util::{now_unix, sha256_hex, write_atomic};

/// FlClashX default when the provider sends no `profile-update-interval`.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(24 * 3600);
/// Never poll a panel more often than this, whatever the provider asks for.
pub const MIN_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Real subscriptions are well below 1 MiB; this bounds parser memory.
const MAX_SUBSCRIPTION_BYTES: usize = 4 * 1024 * 1024;
/// Upper bound for `mihomo -t` (it may try to download missing geodata).
const VALIDATION_TIMEOUT: Duration = Duration::from_secs(60);

pub struct Updater {
    pub config: Config,
    pub store: Store,
    pub emulation: Emulation,
    pub secret: String,
    client: http::Client,
    crons: Vec<crate::schedule::Cron>,
}

pub struct Restored {
    /// Unix time of the next scheduled update (may be in the past: overdue).
    pub next_update: Option<u64>,
}

pub enum Outcome {
    /// A new config was written (`changed = false`: same body, nothing rewritten).
    Applied {
        changed: bool,
        info: ProviderInfo,
        proxies: usize,
    },
    /// The response was not applied; the previous config stays.
    Kept(Problem),
}

impl Updater {
    pub fn new(config: Config) -> Result<Self> {
        let store = Store::open(&config.data_dir)?;
        let identity = Identity::load(&config, &store)?;
        let emulation = Emulation::new(&config, identity)?;
        let secret = match &config.core.secret {
            Some(secret) => secret.clone(),
            None => store.secret()?,
        };
        let proxy = config
            .subscription
            .proxy
            .as_deref()
            .map(Url::parse)
            .transpose()?;
        if let Ok(url) = config.subscription_url().and_then(Url::parse)
            && url.scheme == Scheme::Http
        {
            crate::warn!(
                "the subscription URL uses plain http://: anyone on the path can read your token and alter the config"
            );
        }
        Ok(Self {
            store,
            emulation,
            secret,
            client: http::Client {
                proxy,
                max_body: MAX_SUBSCRIPTION_BYTES,
                ..http::Client::default()
            },
            crons: config
                .update
                .cron
                .iter()
                .map(|c| c.parse())
                .collect::<Result<_>>()?,
            config,
        })
    }

    /// Cache key: a cached body is only reused for the same URL and client.
    fn source_id(&self) -> Result<String> {
        let url = self.config.subscription_url()?;
        Ok(format!(
            "{}:{}",
            self.emulation.kind,
            &sha256_hex(url.as_bytes())[..16]
        ))
    }

    /// The URL to request, honouring a FlClashX `flclashx-newdomain` move.
    pub fn url(&self) -> Result<Url> {
        let configured = self.config.subscription_url()?;
        let meta = self.store.load_meta().unwrap_or_default();
        let effective = match &meta.url_override {
            Some((original, moved)) if original == configured => moved.as_str(),
            _ => configured,
        };
        Url::parse(effective)
    }

    pub fn fetch(&self) -> Result<(subscription::Fetch, Analysis)> {
        let url = self.url()?;
        let fetch = subscription::fetch(&self.client, &self.emulation, &url)?;
        let analysis = self.analyze(&fetch.response);
        Ok((fetch, analysis))
    }

    fn analyze(&self, response: &Response) -> Analysis {
        // Only Koala rejects bodies by Content-Type; FlClashX and Happ look at the body.
        subscription::analyze(response, self.emulation.kind == ClientKind::Koala)
    }

    /// Unix time of the next scheduled update for a subscription fetched at
    /// `fetched_at`: the earliest of the interval and every cron expression.
    /// Times already in the past mean "overdue" (e.g. the host was off at 05:00).
    /// `None` when only manual/start-up updates are configured.
    pub fn next_update(&self, fetched_at: u64, info: &ProviderInfo) -> Option<u64> {
        let by_interval = match self.config.update.interval {
            Interval::Off => None,
            Interval::Fixed(d) => Some(d),
            Interval::Auto => Some(info.update_interval.unwrap_or(DEFAULT_INTERVAL)),
        }
        .map(|d| fetched_at.saturating_add(d.max(MIN_INTERVAL).as_secs()));
        let by_cron = self
            .crons
            .iter()
            .filter_map(|c| c.next_after(fetched_at, crate::schedule::local_time))
            .min();
        by_interval.into_iter().chain(by_cron).min()
    }

    /// Fetches the subscription and writes a new config if it is usable.
    /// Every outcome, errors included, is recorded in the metadata.
    pub fn update(&self) -> Result<Outcome> {
        let mut meta = self.store.load_meta().unwrap_or_default();
        meta.checked_at = now_unix();
        meta.update_seq = meta.update_seq.wrapping_add(1);
        let result = self.try_update(&mut meta);
        match &result {
            Ok(Outcome::Applied { .. }) => meta.last_error = None,
            Ok(Outcome::Kept(problem)) => meta.last_error = Some(problem.to_string()),
            Err(e) => meta.last_error = Some(format!("{e:#}")),
        }
        self.store.save_meta(&meta)?;
        result
    }

    fn try_update(&self, meta: &mut SubscriptionMeta) -> Result<Outcome> {
        let (fetch, analysis) = self.fetch()?;
        let Some(content) = analysis.usable(self.config.subscription.accept_stub) else {
            let problem = analysis
                .problem
                .clone()
                .unwrap_or_else(|| Problem::Invalid("unusable response".into()));
            return Ok(Outcome::Kept(problem));
        };
        for note in &content.notes {
            crate::warn!("conversion: {note}");
        }
        self.follow_new_domain(&fetch, &analysis, meta);
        let body = &fetch.response.body;
        let source = self.source_id()?;
        let changed = meta.format.is_empty()
            || meta.client != source
            || self.store.load_body().as_deref() != Some(body.as_slice());
        if changed {
            let built = match self.build(content) {
                Ok(built) => built,
                Err(e) => return Ok(Outcome::Kept(Problem::Invalid(format!("{e:#}")))),
            };
            if let Err(reason) = self.validate(&built) {
                return Ok(Outcome::Kept(Problem::Invalid(reason)));
            }
            self.backup();
            self.write_built(&built)?;
            self.store.save_body(body)?;
        }
        meta.fetched_at = meta.checked_at;
        meta.headers = fetch.response.headers.clone();
        meta.client = source;
        meta.format = content.format.as_str().to_owned();
        meta.proxies = content.endpoints.len();
        meta.panel_hosts = fetch.hosts();
        meta.panel_ips = fetch.peers.clone();
        Ok(Outcome::Applied {
            changed,
            info: analysis.info,
            proxies: meta.proxies,
        })
    }

    /// FlClashX replaces the subscription host when the provider sends
    /// `flclashx-newdomain` (lib/models/profile.dart). Only honoured from a usable
    /// HTTPS response and only for a bare host name.
    fn follow_new_domain(
        &self,
        fetch: &subscription::Fetch,
        analysis: &Analysis,
        meta: &mut SubscriptionMeta,
    ) {
        let Some(domain) = analysis.info.new_domain.as_deref() else {
            return;
        };
        if !self.emulation.follows_new_domain() {
            return;
        }
        if fetch.url.scheme != Scheme::Https || !is_plain_host(domain) {
            crate::warn!(
                "ignoring flclashx-newdomain {domain:?}: not a host name or not over https"
            );
            return;
        }
        let Ok(current) = self.url() else { return };
        if current.host.eq_ignore_ascii_case(domain) {
            return;
        }
        let (Ok(configured), Ok(moved)) =
            (self.config.subscription_url(), current.with_host(domain))
        else {
            return;
        };
        crate::info!("provider moved the subscription to {domain}");
        meta.url_override = Some((configured.to_owned(), moved.to_string()));
    }

    /// Rebuilds the config from the cached body (current settings applied).
    /// `None` when there is no usable cache for the configured URL and client.
    pub fn restore(&self) -> Result<Option<Restored>> {
        let Some((meta, analysis)) = self.cached()? else {
            return Ok(None);
        };
        // It was accepted when fetched, possibly via accept_stub: don't re-judge it.
        let Some(content) = analysis.content.as_ref() else {
            return Ok(None);
        };
        let built = self.build(content)?;
        self.write_built(&built)?;
        Ok(Some(Restored {
            next_update: self.next_update(meta.fetched_at, &analysis.info),
        }))
    }

    /// The cached subscription for the configured URL and client, re-analysed.
    fn cached(&self) -> Result<Option<(SubscriptionMeta, Analysis)>> {
        let (Some(meta), Some(body)) = (self.store.load_meta(), self.store.load_body()) else {
            return Ok(None);
        };
        if meta.client != self.source_id()? {
            crate::info!("subscription URL or client changed; ignoring the cached subscription");
            return Ok(None);
        }
        let response = Response {
            status: 200,
            reason: "OK".into(),
            headers: meta.headers.clone(),
            body,
            peer: None,
        };
        let analysis = self.analyze(&response);
        Ok(Some((meta, analysis)))
    }

    /// Builds (without writing) the mihomo config from the cached subscription.
    pub fn render(&self) -> Result<Built> {
        let (_, analysis) = self
            .cached()?
            .context("no cached subscription yet: run `mihomyak update` first")?;
        let content = analysis
            .content
            .context("the cached subscription is unusable")?;
        self.build(&content)
    }

    fn build(&self, content: &subscription::Content) -> Result<Built> {
        let mode = self
            .store
            .mode()
            .unwrap_or_else(|| self.config.core.mode.clone());
        let meta = self.store.load_meta().unwrap_or_default();
        let params = Params {
            secret: &self.secret,
            mode: &mode,
            panel_hosts: &meta.panel_hosts,
            panel_ips: &meta.panel_ips,
        };
        crate::profile::build(content, &self.config, &params)
    }

    /// Records when the supervisor will update next (shown by `mihomyak status`).
    pub fn record_next_update(&self, at: Option<u64>) {
        if let Some(mut meta) = self.store.load_meta() {
            meta.next_update_at = at;
            if let Err(e) = self.store.save_meta(&meta) {
                crate::warn!("could not save subscription metadata: {e:#}");
            }
        }
    }

    fn write_built(&self, built: &Built) -> Result<()> {
        for warning in &built.warnings {
            crate::warn!("{warning}");
        }
        if let Some(provider) = &built.provider {
            write_atomic(&self.provider_path(), provider).context("write provider file")?;
        }
        write_atomic(&self.store.mihomo_config(), built.config_yaml.as_bytes())
            .context("write mihomo config")
    }

    fn provider_path(&self) -> PathBuf {
        self.store.mihomo_home().join(crate::profile::PROVIDER_FILE)
    }

    /// Files replaced by an update, with their `*.prev` backups.
    fn replaced_files(&self) -> [PathBuf; 3] {
        [
            self.store.mihomo_config(),
            self.provider_path(),
            self.store.body_path(),
        ]
    }

    fn backup(&self) {
        for path in self.replaced_files() {
            if path.exists() {
                let _ = std::fs::copy(&path, prev(&path));
            }
        }
    }

    /// Puts back the files replaced by the last update, after the running core
    /// rejected the new config. The metadata keeps the error for `status`.
    pub fn rollback(&self, reason: &str) -> Result<()> {
        for path in self.replaced_files() {
            let backup = prev(&path);
            if backup.exists() {
                std::fs::rename(&backup, &path)
                    .with_context(|| format!("restore {}", path.display()))?;
            }
        }
        if let Some(mut meta) = self.store.load_meta() {
            // Force the next update to rebuild even if the body is the same.
            meta.format.clear();
            meta.last_error = Some(format!("mihomo rejected the new config: {reason}"));
            self.store.save_meta(&meta)?;
        }
        Ok(())
    }

    /// Runs `mihomo -t` on a built config: `Ok(None)` if accepted, `Ok(Some(log))`
    /// if rejected, `Err` if the check itself could not run.
    pub fn check_config(&self, built: &Built) -> Result<Option<String>> {
        let bin = crate::core::resolve_bin(&self.config, &self.store);
        if !bin.is_file() && crate::core::which(&bin).is_none() {
            anyhow::bail!("mihomo binary not found");
        }
        let home = self.store.mihomo_home();
        let candidate = home.join(".candidate.yaml");
        write_atomic(&candidate, built.config_yaml.as_bytes())
            .context("write the candidate config")?;
        let result = run_check(&bin, &home, &candidate);
        let _ = std::fs::remove_file(&candidate);
        result
    }

    /// Runs `mihomo -t` on a built config before it may replace the live one.
    /// Returns the rejection reason; problems of the check itself (no binary,
    /// geodata download failing, timeout) are logged and let the config through.
    fn validate(&self, built: &Built) -> Result<(), String> {
        match self.check_config(built) {
            Ok(None) => Ok(()),
            Ok(Some(log)) => {
                let reason = last_error_line(&log);
                if reason.to_ascii_lowercase().contains("geo") || reason.contains("download") {
                    crate::warn!("config check could not load geodata ({reason}); applying anyway");
                    Ok(())
                } else {
                    Err(format!("mihomo rejected the generated config: {reason}"))
                }
            }
            Err(e) => {
                crate::warn!("config check skipped: {e:#}");
                Ok(())
            }
        }
    }
}

fn prev(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".prev");
    path.with_file_name(name)
}

/// `Ok(None)` if mihomo accepts the config, `Ok(Some(output))` if it rejects it.
/// Output goes to a file, not a pipe: a chatty run must not block on a full pipe
/// while we wait for it.
fn run_check(bin: &Path, home: &Path, config: &Path) -> Result<Option<String>> {
    let log_path = home.join(".candidate.log");
    let log = std::fs::File::create(&log_path).context("create the check log")?;
    let mut child = crate::core::command(bin)
        .arg("-t")
        .arg("-d")
        .arg(home)
        .arg("-f")
        .arg(config)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()
        .with_context(|| format!("run {} -t", bin.display()))?;
    let deadline = Instant::now() + VALIDATION_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("mihomo -t did not finish within {VALIDATION_TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let output = std::fs::read(&log_path).unwrap_or_default();
    let _ = std::fs::remove_file(&log_path);
    if status.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&output).into_owned()))
}

/// The most useful line of a failed `mihomo -t` run.
fn last_error_line(log: &str) -> String {
    let line = log
        .lines()
        .rev()
        .find(|l| l.contains("level=error") || l.contains("level=fatal"))
        .or_else(|| log.lines().rev().find(|l| !l.trim().is_empty()))
        .unwrap_or("unknown error");
    let msg = line
        .split_once("msg=\"")
        .map(|(_, m)| m.trim_end_matches('"'))
        .unwrap_or(line);
    crate::util::sanitize(msg)
}

/// A bare DNS host name (optionally `:port`): no scheme, path, credentials or IP literal.
fn is_plain_host(value: &str) -> bool {
    let host = match value.rsplit_once(':') {
        Some((h, port)) if port.parse::<u16>().is_ok() => h,
        _ => value,
    };
    !host.is_empty()
        && host.len() <= 253
        && host.contains('.')
        && host.parse::<IpAddr>().is_err()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
}

/// One line describing provider info, for logs.
pub fn describe(info: &ProviderInfo, proxies: usize) -> String {
    let mut parts = vec![format!("{proxies} proxies")];
    if let Some(title) = &info.title {
        parts.insert(0, format!("\"{title}\""));
    }
    if let Some(usage) = &info.usage {
        parts.push(format!(
            "used {} of {}",
            crate::util::fmt_bytes(usage.used()),
            usage.total_display()
        ));
        if usage.expire > 0 {
            parts.push(format!("expires {}", crate::util::fmt_date(usage.expire)));
        }
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_hosts() {
        assert!(is_plain_host("new.example.com"));
        assert!(is_plain_host("new.example.com:8443"));
        assert!(!is_plain_host("https://new.example.com"));
        assert!(!is_plain_host("new.example.com/path"));
        assert!(!is_plain_host("127.0.0.1"));
        assert!(!is_plain_host("localhost"));
        assert!(!is_plain_host("user@host.com"));
        assert!(!is_plain_host(""));
    }

    #[test]
    fn extracts_mihomo_errors() {
        let log = "time=\"x\" level=info msg=\"Start\"\ntime=\"x\" level=error msg=\"proxy 0: 'reality-opts' has unset fields: public-key\"\nconfiguration file x test failed\n";
        assert_eq!(
            last_error_line(log),
            "proxy 0: 'reality-opts' has unset fields: public-key"
        );
        assert_eq!(last_error_line("boom\n"), "boom");
    }

    #[test]
    fn backup_names() {
        assert_eq!(
            prev(Path::new("/d/config.yaml")),
            Path::new("/d/config.yaml.prev")
        );
    }
}
