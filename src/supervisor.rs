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
    next_update: Instant,
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
            next_update: Instant::now(),
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
        self.start_core()?;
        loop {
            let deadline = self
                .restart_at
                .map_or(self.next_update, |r| r.min(self.next_update));
            match self
                .events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(Event::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(Event::Update) => {
                    crate::info!("update requested");
                    self.update();
                }
                Ok(Event::Child) => self.reap(),
                Err(RecvTimeoutError::Timeout) => {
                    let now = Instant::now();
                    if self.restart_at.is_some_and(|at| at <= now) {
                        self.restart_at = None;
                        if let Err(e) = self.start_core() {
                            crate::error!("{e:#}");
                            self.schedule_restart();
                        }
                    }
                    if self.next_update <= now {
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
        if let Some(due_in) = self.updater.restore()? {
            crate::info!(
                "using the cached subscription, next update in {}",
                crate::util::fmt_duration(due_in)
            );
            self.next_update = Instant::now() + due_in;
            return Ok(true);
        }
        loop {
            if self.update() {
                return Ok(true);
            }
            let wait = self.next_update.saturating_duration_since(Instant::now());
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
                let interval = self.updater.interval(&info);
                self.next_update = Instant::now() + interval;
                crate::info!(
                    "subscription {}: {}; next update in {}",
                    if changed { "updated" } else { "unchanged" },
                    updater::describe(&info, proxies),
                    crate::util::fmt_duration(interval)
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
        let interval = self.updater.interval(&Default::default());
        self.next_update = Instant::now() + backoff.min(MAX_RETRY_BACKOFF).min(interval);
    }

    fn start_core(&mut self) -> Result<()> {
        // Re-render from the cache so runtime choices (e.g. `mihomyak mode`) survive
        // a core crash/restart even before the next subscription update.
        if let Err(e) = self.updater.restore() {
            crate::warn!("could not refresh config.yaml from the cache: {e:#}");
        }
        let home = self.updater.store.mihomo_home();
        self.core = Some(CoreProcess::spawn(&self.bin, &home, &self.updater.config)?);
        Ok(())
    }

    /// Applies the freshly written config: hot reload, or restart as a fallback.
    fn reload_core(&mut self) {
        if self.core.is_none() {
            return;
        }
        let path = self.updater.store.mihomo_config();
        match self.api.reload(&path) {
            Ok(()) => crate::info!("mihomo reloaded the new config"),
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
