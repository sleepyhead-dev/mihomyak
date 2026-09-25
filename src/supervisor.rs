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

use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use signal_hook::consts::{SIGCHLD, SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use crate::api::Api;
use crate::config::Config;
use crate::core::{self, CoreProcess};
use crate::updater::{self, Outcome, Updater};
use crate::util::now_unix;

const STOP_GRACE: Duration = Duration::from_secs(10);
const MAX_RESTART_BACKOFF: Duration = Duration::from_secs(60);
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
    let store = updater.store.clone();
    if let Some(pid) = store.supervisor_pid() {
        bail!(
            "another supervisor (pid {pid}) already uses {}",
            store.root().display()
        );
    }
    store.write_pid()?;
    let result = Supervisor::new(updater)?.run();
    store.remove_pid();
    result
}

struct Supervisor {
    updater: Updater,
    api: Api,
    bin: std::path::PathBuf,
    core: Option<CoreProcess>,
    events: mpsc::Receiver<Event>,
    /// Unix time of the next subscription update (`None`: only on demand).
    next_update: Option<u64>,
    update_failures: u32,
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
            next_update: Some(now_unix()),
            update_failures: 0,
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
        if !self.prepare()? {
            return Ok(());
        }
        if let Some(dir) = &self.updater.config.core.geodata_dir {
            core::seed_geodata(dir, &self.updater.store.mihomo_home());
        }
        self.start_core()?;
        loop {
            match self.events.recv_timeout(self.wait_time()) {
                Ok(Event::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(Event::Update) => {
                    crate::info!("update requested");
                    self.update();
                }
                Ok(Event::Child) => self.reap(),
                Err(RecvTimeoutError::Timeout) => {
                    if self.restart_at.is_some_and(|at| at <= Instant::now()) {
                        self.restart_at = None;
                        if let Err(e) = self.start_core() {
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
                describe_next(next)
            );
            self.set_next_update(next);
            return Ok(true);
        }
        loop {
            if self.update() {
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
                if changed || self.core.is_none() {
                    self.reload_core();
                }
                true
            }
            Ok(Outcome::Kept(problem)) => {
                crate::warn!("keeping the current config: {problem}");
                self.schedule_retry();
                false
            }
            Err(e) => {
                crate::warn!("subscription update failed: {e:#}");
                self.schedule_retry();
                false
            }
        }
    }

    fn schedule_retry(&mut self) {
        self.update_failures += 1;
        let backoff = Duration::from_secs(60) * 2u32.saturating_pow(self.update_failures - 1);
        self.set_next_update(Some(now_unix() + backoff.min(MAX_RETRY_BACKOFF).as_secs()));
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

    fn start_core(&mut self) -> Result<()> {
        // Re-render from the cache so runtime choices (e.g. `mihomyak mode`) survive
        // a core crash/restart even before the next subscription update.
        if let Err(e) = self.updater.restore() {
            crate::warn!("could not refresh config.yaml from the cache: {e:#}");
        }
        let home = self.updater.store.mihomo_home();
        self.core = Some(CoreProcess::spawn(&self.bin, &home, &self.updater.config)?);
        self.select_default_groups();
        Ok(())
    }

    /// `[[groups]] default = true`: make selectors use these groups. mihomo
    /// restores the previous choice from cache.db (store-selected), so the default
    /// is applied explicitly whenever the core starts or gets a new subscription.
    fn select_default_groups(&self) {
        let defaults: Vec<&str> = self
            .updater
            .config
            .groups
            .iter()
            .filter(|g| g.default)
            .map(|g| g.name.as_str())
            .collect();
        if defaults.is_empty() {
            return;
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        let snapshot = loop {
            match self.api.snapshot() {
                Ok(s) if !s.groups.is_empty() => break s,
                _ if Instant::now() >= deadline => {
                    crate::warn!("mihomo API not ready; default groups not selected");
                    return;
                }
                _ => std::thread::sleep(Duration::from_millis(200)),
            }
        };
        for group in snapshot
            .groups
            .iter()
            .filter(|g| g.selectable() && g.name != "GLOBAL")
        {
            let Some(default) = group
                .members
                .iter()
                .find(|m| defaults.contains(&m.as_str()))
            else {
                continue;
            };
            if group.now.as_ref() == Some(default) {
                continue;
            }
            match self.api.select(&group.name, default) {
                Ok(()) => crate::info!("{} → {default} (default group)", group.name),
                Err(e) => crate::warn!("could not select {default} in {}: {e:#}", group.name),
            }
        }
    }

    /// Applies the freshly written config: hot reload, or restart as a fallback.
    fn reload_core(&mut self) {
        if self.core.is_none() {
            return;
        }
        let path = self.updater.store.mihomo_config();
        match self.api.reload(&path) {
            Ok(()) => {
                crate::info!("mihomo reloaded the new config");
                self.select_default_groups();
            }
            Err(e) => {
                crate::warn!("hot reload failed ({e:#}); restarting mihomo");
                if let Some(core) = self.core.take() {
                    core.stop(STOP_GRACE);
                }
                if let Err(e) = self.start_core() {
                    crate::error!("{e:#}");
                    self.schedule_restart();
                }
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
