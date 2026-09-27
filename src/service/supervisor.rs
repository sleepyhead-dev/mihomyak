//! `mihomyak run`: keeps mihomo running on a fresh subscription.
//!
//! One thread waits for signals, the main thread runs a small event loop:
//!
//! * `SIGTERM`/`SIGINT` — stop mihomo gracefully and exit;
//! * `SIGHUP`           — update the subscription now (`mihomyak update` sends it);
//! * `SIGCHLD`          — mihomo died: restart it with exponential backoff;
//! * timer              — scheduled subscription update or pending restart.
//!
//! No async runtime: the supervisor idles in `recv_timeout` (≈4 MiB RSS measured).

use std::collections::VecDeque;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use signal_hook::consts::{SIGCHLD, SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use crate::mihomo::api::{Api, Rejected};
use crate::config::Config;
use crate::mihomo::core::{self, CoreProcess};
use crate::service::updater::{self, Outcome, Updater};
use crate::util::now_unix;

/// Below Docker's default 10 s stop timeout, so mihomo gets to clean up TUN routes
/// before the container runtime resorts to SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(8);
/// How long a freshly started core may take before its API answers.
const API_READY_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_RESTART_BACKOFF: Duration = Duration::from_secs(60);
/// First retry after a failed update, doubling up to [`MAX_RETRY_BACKOFF`].
const RETRY_BACKOFF: Duration = Duration::from_secs(60);
/// First retry after a network error while there is no usable config yet.
const BOOT_RETRY_BACKOFF: Duration = Duration::from_secs(5);
const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(3600);
/// A core that ran this long is considered healthy again (backoff resets).
const HEALTHY_UPTIME: Duration = Duration::from_secs(60);

enum Event {
    Stop,
    Update,
    Child,
}

pub fn run(config: Config) -> Result<()> {
    let updater = Updater::new(config)?;
    let _lock = updater.store.lock_supervisor()?;
    let gateway = &updater.config.gateway;
    if gateway.enable
        && let Some(leak) = crate::gateway::dns_leak()
    {
        if !gateway.allow_dns_leak {
            bail!("{leak}");
        }
        crate::warn!("{leak}");
    }
    // Before anything else: from here on nothing may bypass mihomo. Dropped (rules
    // removed) only when `run` returns.
    let _kill_switch = gateway
        .kill_switch
        .then(crate::gateway::killswitch::enable)
        .transpose()?;
    Supervisor::new(updater)?.run()
}

struct Supervisor {
    updater: Updater,
    api: Api,
    bin: std::path::PathBuf,
    core: Option<CoreProcess>,
    events: mpsc::Receiver<Event>,
    /// Events received while waiting for something else, handled next.
    deferred: VecDeque<Event>,
    /// Unix time of the next subscription update (`None`: only on demand).
    next_update: Option<u64>,
    update_failures: u32,
    /// No usable subscription yet (first start without a cache).
    bootstrapping: bool,
    restart_at: Option<Instant>,
    restart_backoff: Duration,
}

impl Supervisor {
    fn new(updater: Updater) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let mut signals =
            Signals::new([SIGTERM, SIGINT, SIGHUP, SIGCHLD]).context("install signal handlers")?;
        thread::Builder::new()
            .name("signals".into())
            .stack_size(64 * 1024)
            .spawn(move || {
                for signal in signals.forever() {
                    let event = match signal {
                        SIGHUP => Event::Update,
                        SIGCHLD => Event::Child,
                        _ => Event::Stop,
                    };
                    if tx.send(event).is_err() {
                        break;
                    }
                }
            })?;
        let api = Api::new(&updater.config.core.controller, &updater.secret)?;
        let bin = core::resolve_bin(&updater.config, &updater.store);
        Ok(Self {
            updater,
            api,
            bin,
            core: None,
            events: rx,
            deferred: VecDeque::new(),
            next_update: Some(now_unix()),
            update_failures: 0,
            bootstrapping: false,
            restart_at: None,
            restart_backoff: Duration::from_secs(1),
        })
    }

    fn run(mut self) -> Result<()> {
        crate::info!(
            "mihomyak {} as {} (UA {:?}), data in {}",
            env!("CARGO_PKG_VERSION"),
            self.updater.emulation.kind,
            self.updater.emulation.user_agent(),
            self.updater.store.root().display()
        );
        crate::info!(
            "subscription {}",
            crate::subscription::redact(&self.updater.url()?)
        );
        if !self.updater.config.update.cron.is_empty() {
            crate::info!(
                "cron schedules use local time UTC{} (set TZ to change it)",
                crate::service::schedule::utc_offset(now_unix())
            );
        }
        if !self.prepare()? {
            return Ok(());
        }
        if let Some(dir) = &self.updater.config.core.geodata_dir {
            core::seed_geodata(dir, &self.updater.store.mihomo_home());
        }
        if self.api.version().is_ok() {
            bail!(
                "something already answers on the mihomo controller {}: stop the other mihomo or change core.controller",
                self.updater.config.core.controller
            );
        }
        // prepare() has just written config.yaml: no need to rebuild it.
        self.start_core(false)?;
        loop {
            let event = match self.deferred.pop_front() {
                Some(event) => Ok(event),
                None => self.events.recv_timeout(self.wait_time()),
            };
            match event {
                Ok(Event::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(Event::Update) => {
                    crate::info!("update requested");
                    self.update();
                }
                Ok(Event::Child) => self.reap(),
                Err(RecvTimeoutError::Timeout) => {
                    if self.restart_at.is_some_and(|at| at <= Instant::now()) {
                        self.restart_at = None;
                        if let Err(e) = self.start_core(true) {
                            crate::error!("{e:#}");
                            self.schedule_restart();
                        }
                    }
                    if self.next_update.is_some_and(|at| at <= now_unix()) {
                        self.update();
                    }
                }
            }
        }
        crate::info!("shutting down");
        if let Some(core) = self.core.take() {
            core.stop(STOP_GRACE);
        }
        Ok(())
    }

    /// Makes sure a config exists before mihomo starts: the cached subscription
    /// if there is one (like the real clients, no refetch until it is due),
    /// otherwise fetch until success. Returns `false` if asked to stop meanwhile.
    fn prepare(&mut self) -> Result<bool> {
        if let Some(restored) = self.updater.restore()? {
            // Start on the cache right away; `on_start` refreshes it immediately after.
            let next = if self.updater.config.update.on_start {
                Some(now_unix())
            } else {
                restored.next_update
            };
            crate::info!(
                "using the cached subscription; next update {}",
                if self.updater.config.update.on_start {
                    "now (update.on_start)".to_owned()
                } else {
                    describe_next(next)
                }
            );
            self.set_next_update(next);
            return Ok(true);
        }
        self.bootstrapping = true;
        loop {
            if self.update() {
                self.bootstrapping = false;
                return Ok(true);
            }
            let wait = self.wait_time();
            crate::info!(
                "no usable subscription yet, retrying in {}",
                crate::util::fmt_duration(wait)
            );
            match self.events.recv_timeout(wait) {
                Ok(Event::Stop) | Err(RecvTimeoutError::Disconnected) => return Ok(false),
                _ => {}
            }
        }
    }

    /// Runs one update and schedules the next. Returns whether a config was applied.
    fn update(&mut self) -> bool {
        match self.updater.update() {
            Ok(Outcome::Applied {
                changed,
                info,
                proxies,
            }) => {
                self.update_failures = 0;
                let next = self.updater.next_update(now_unix(), &info);
                self.set_next_update(next);
                crate::info!(
                    "subscription {}: {}; next update {}",
                    if changed { "updated" } else { "unchanged" },
                    updater::describe(&info, proxies),
                    describe_next(next)
                );
                if let Some(announce) = &info.announce {
                    crate::info!("provider announcement: {announce}");
                }
                if changed && !self.reload_core() {
                    self.schedule_retry(false);
                    return false;
                }
                true
            }
            Ok(Outcome::Kept(problem)) => {
                if self.bootstrapping {
                    crate::warn!("subscription not applied: {problem}");
                } else {
                    crate::warn!("keeping the current config: {problem}");
                }
                self.schedule_retry(false);
                false
            }
            Err(e) => {
                crate::warn!("subscription update failed: {e:#}");
                // Without any config nothing runs (a gateway has no network at
                // all), and at boot DNS or the network are often just not ready.
                self.schedule_retry(self.bootstrapping);
                false
            }
        }
    }

    fn schedule_retry(&mut self, soon: bool) {
        self.update_failures += 1;
        let backoff = retry_backoff(self.update_failures, soon);
        self.set_next_update(Some(now_unix() + backoff.as_secs()));
    }

    fn set_next_update(&mut self, at: Option<u64>) {
        self.next_update = at;
        self.updater.record_next_update(at);
    }

    /// Time until the earliest pending event (update or core restart).
    fn wait_time(&self) -> Duration {
        let update = self
            .next_update
            .map(|at| Duration::from_secs(at.saturating_sub(now_unix())));
        let restart = self
            .restart_at
            .map(|at| at.saturating_duration_since(Instant::now()));
        // Wake at least hourly: wall-clock jumps (NTP after boot) must not strand a cron run.
        [update, restart, Some(Duration::from_secs(3600))]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or_default()
    }

    /// `rebuild`: re-render config.yaml from the cache first, so runtime choices
    /// (e.g. `mihomyak mode`) survive a core crash even before the next update.
    fn start_core(&mut self, rebuild: bool) -> Result<()> {
        if rebuild && let Err(e) = self.updater.restore() {
            crate::warn!("could not refresh config.yaml from the cache: {e:#}");
        }
        let home = self.updater.store.mihomo_home();
        self.core = Some(CoreProcess::spawn(&self.bin, &home, &self.updater.config)?);
        self.select_default_groups();
        Ok(())
    }

    /// Waits until the core's API lists groups. Signals arriving meanwhile are
    /// kept for the main loop; a stop request or a dead core ends the wait early.
    fn wait_for_api(&mut self) -> Option<crate::mihomo::api::Snapshot> {
        let deadline = Instant::now() + API_READY_TIMEOUT;
        loop {
            match self.api.snapshot() {
                Ok(s) if !s.groups.is_empty() => return Some(s),
                _ if Instant::now() >= deadline => {
                    crate::warn!("mihomo API not ready; default groups not selected");
                    return None;
                }
                _ => {}
            }
            match self.events.recv_timeout(Duration::from_millis(200)) {
                Ok(event) => {
                    let stop = matches!(event, Event::Stop);
                    self.deferred.push_back(event);
                    let exited = self
                        .core
                        .as_mut()
                        .is_none_or(|core| matches!(core.try_wait(), Ok(Some(_))));
                    if stop || exited {
                        return None;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    /// `[[groups]] default = true`: make selectors use these groups. mihomo
    /// restores the previous choice from cache.db (store-selected), so the default
    /// is applied explicitly whenever the core starts or gets a new subscription.
    fn select_default_groups(&mut self) {
        let defaults: Vec<String> = self
            .updater
            .config
            .groups
            .iter()
            .filter(|g| g.default)
            .map(|g| g.name.clone())
            .collect();
        if defaults.is_empty() {
            return;
        }
        let Some(snapshot) = self.wait_for_api() else {
            return;
        };
        for group in snapshot.groups.iter().filter(|g| g.is_user_selector()) {
            let Some(default) = group.members.iter().find(|m| defaults.contains(m)) else {
                continue;
            };
            if group.now.as_ref() == Some(default) {
                continue;
            }
            match self.api.select(&group.name, default) {
                Ok(()) => crate::info!(
                    "{} → {default} (default group)",
                    crate::util::sanitize(&group.name)
                ),
                Err(e) => crate::warn!("could not select {default} in {}: {e:#}", group.name),
            }
        }
    }

    /// Applies the freshly written config by hot reload. If mihomo refuses it, the
    /// previous files are restored (the core keeps running the old config) and
    /// `false` is returned. If the API is unreachable, the core is restarted.
    fn reload_core(&mut self) -> bool {
        if self.core.is_none() {
            // Crashed and waiting for a restart, which will pick the new config up.
            return true;
        }
        let path = self.updater.store.mihomo_config();
        match self.api.reload(&path) {
            Ok(()) => {
                crate::info!("mihomo reloaded the new config");
                self.select_default_groups();
                true
            }
            Err(e) if e.downcast_ref::<Rejected>().is_some() => {
                crate::error!("mihomo rejected the new config ({e:#}); keeping the previous one");
                if let Err(e) = self.updater.rollback(&format!("{e:#}")) {
                    crate::error!("rollback failed: {e:#}");
                }
                false
            }
            Err(e) => {
                crate::warn!("hot reload failed ({e:#}); restarting mihomo");
                if let Some(core) = self.core.take() {
                    core.stop(STOP_GRACE);
                }
                if let Err(e) = self.start_core(false) {
                    crate::error!("{e:#}");
                    self.schedule_restart();
                }
                true
            }
        }
    }

    fn reap(&mut self) {
        if let Some(core) = &mut self.core {
            match core.try_wait() {
                Ok(Some(status)) => {
                    let uptime = core.uptime();
                    self.core = None;
                    if uptime >= HEALTHY_UPTIME {
                        self.restart_backoff = Duration::from_secs(1);
                    }
                    crate::error!(
                        "mihomo exited ({status}) after {}",
                        crate::util::fmt_duration(uptime)
                    );
                    self.schedule_restart();
                }
                Ok(None) => {}
                Err(e) => crate::warn!("waiting for mihomo: {e}"),
            }
        }
        reap_orphans();
    }

    fn schedule_restart(&mut self) {
        crate::info!(
            "restarting mihomo in {}",
            crate::util::fmt_duration(self.restart_backoff)
        );
        self.restart_at = Some(Instant::now() + self.restart_backoff);
        self.restart_backoff = (self.restart_backoff * 2).min(MAX_RESTART_BACKOFF);
    }
}

fn describe_next(at: Option<u64>) -> String {
    match at {
        None => "on demand only (interval off, no cron)".into(),
        Some(at) if at <= now_unix() => "now (overdue)".into(),
        Some(at) => format!(
            "at {} (in {})",
            crate::util::fmt_timestamp(at),
            crate::util::fmt_duration(Duration::from_secs(at - now_unix()))
        ),
    }
}

/// Delay before retry number `failures` (from 1): doubling from 1 minute, or
/// from 5 seconds when `soon`, up to 1 hour.
fn retry_backoff(failures: u32, soon: bool) -> Duration {
    let first = if soon {
        BOOT_RETRY_BACKOFF
    } else {
        RETRY_BACKOFF
    };
    first
        .saturating_mul(2u32.saturating_pow(failures.saturating_sub(1)))
        .min(MAX_RETRY_BACKOFF)
}

/// As PID 1 in a container we inherit orphaned processes and must reap them.
fn reap_orphans() {
    if std::process::id() != 1 {
        return;
    }
    loop {
        let mut status = 0;
        // SAFETY: non-blocking waitpid on any child; no memory is shared.
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid <= 0 {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_backoff_doubles_up_to_an_hour() {
        let secs = |failures, soon| retry_backoff(failures, soon).as_secs();
        assert_eq!(
            [1, 2, 3, 7, 8].map(|n| secs(n, false)),
            [60, 120, 240, 3600, 3600]
        );
        // No config yet and a network error: the first retries come sooner.
        assert_eq!([1, 2, 3, 4, 5].map(|n| secs(n, true)), [5, 10, 20, 40, 80]);
        assert_eq!(secs(u32::MAX, true), 3600);
    }
}
