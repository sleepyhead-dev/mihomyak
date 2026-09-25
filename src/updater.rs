//! Subscription update pipeline shared by the supervisor and one-shot commands:
//! fetch → analyse → build config → write files → record metadata.
//!
//! The updater never touches the mihomo process; callers decide whether to reload.

use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::{Config, Interval};
use crate::emulation::Emulation;
use crate::http::{self, Response, Url};
use crate::identity::Identity;
use crate::store::{Store, SubscriptionMeta};
use crate::subscription::{self, Analysis, Problem, ProviderInfo};
use crate::util::{now_unix, sha256_hex};

/// FlClashX default when the provider sends no `profile-update-interval`.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(24 * 3600);
/// Never poll a panel more often than this, whatever the provider asks for.
const MIN_INTERVAL: Duration = Duration::from_secs(5 * 60);

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
    /// A new config was written (`changed = false`: same body as before).
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
        Ok(Self {
            store,
            emulation,
            secret,
            client: http::Client {
                proxy,
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
        let analysis = subscription::analyze(&fetch.response);
        Ok((fetch, analysis))
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
        .map(|d| fetched_at + d.max(MIN_INTERVAL).as_secs());
        let by_cron = self
            .crons
            .iter()
            .filter_map(|c| c.next_after(fetched_at, crate::schedule::local_time))
            .min();
        by_interval.into_iter().chain(by_cron).min()
    }

    /// Fetches the subscription and writes a new config if it is usable.
    pub fn update(&self) -> Result<Outcome> {
        let mut meta = self.store.load_meta().unwrap_or_default();
        meta.checked_at = now_unix();
        let result = self.fetch();
        let (fetch, analysis) = match result {
            Ok(v) => v,
            Err(e) => {
                meta.last_error = Some(format!("{e:#}"));
                self.store.save_meta(&meta)?;
                return Err(e);
            }
        };
        if let Some(domain) = &analysis.info.new_domain
            && self.emulation.follows_new_domain()
        {
            let configured = self.config.subscription_url()?.to_owned();
            let moved = self.url()?.with_host(domain)?.to_string();
            crate::info!("provider moved the subscription to {domain}");
            meta.url_override = Some((configured, moved));
        }
        let Some(content) = analysis.usable(self.config.subscription.accept_stub) else {
            let problem = analysis
                .problem
                .clone()
                .unwrap_or_else(|| Problem::Invalid("unusable response".into()));
            meta.last_error = Some(problem.to_string());
            self.store.save_meta(&meta)?;
            return Ok(Outcome::Kept(problem));
        };
        for note in &content.notes {
            crate::warn!("conversion: {note}");
        }
        let body = &fetch.response.body;
        let source = self.source_id()?;
        let changed = meta.format.is_empty()
            || meta.client != source
            || self.store.load_body().as_deref() != Some(body.as_slice());
        self.write_config(content)?;
        self.store.save_body(body)?;
        meta = SubscriptionMeta {
            fetched_at: meta.checked_at,
            checked_at: meta.checked_at,
            last_error: None,
            headers: fetch.response.headers.clone(),
            client: source,
            format: content.format.as_str().to_owned(),
            proxies: content.endpoints.len(),
            url_override: meta.url_override,
            next_update_at: meta.next_update_at,
        };
        self.store.save_meta(&meta)?;
        Ok(Outcome::Applied {
            changed,
            info: analysis.info,
            proxies: meta.proxies,
        })
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
        self.write_config(content)?;
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
        };
        let analysis = subscription::analyze(&response);
        Ok(Some((meta, analysis)))
    }

    /// Builds (without writing) the mihomo config from the cached subscription.
    pub fn render(&self) -> Result<crate::profile::Built> {
        let (_, analysis) = self
            .cached()?
            .context("no cached subscription yet: run `mihomyak update` first")?;
        let content = analysis
            .content
            .context("the cached subscription is unusable")?;
        self.build(&content)
    }

    fn build(&self, content: &subscription::Content) -> Result<crate::profile::Built> {
        let mode = self
            .store
            .mode()
            .unwrap_or_else(|| self.config.core.mode.clone());
        crate::profile::build(content, &self.config, &self.secret, &mode)
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

    fn write_config(&self, content: &subscription::Content) -> Result<()> {
        let built = self.build(content)?;
        for warning in &built.warnings {
            crate::warn!("{warning}");
        }
        let home = self.store.mihomo_home();
        if let Some(provider) = &built.provider {
            crate::util::write_atomic(&home.join(crate::profile::PROVIDER_FILE), provider)
                .context("write provider file")?;
        }
        crate::util::write_atomic(&self.store.mihomo_config(), built.config_yaml.as_bytes())
            .context("write mihomo config")
    }
}

/// One line describing provider info, for logs.
pub fn describe(info: &ProviderInfo, proxies: usize) -> String {
    let mut parts = vec![format!("{proxies} proxies")];
    if let Some(title) = &info.title {
        parts.insert(0, format!("\"{title}\""));
    }
    if let Some(usage) = &info.usage {
        let total = if usage.total == 0 {
            "∞".to_owned()
        } else {
            crate::util::fmt_bytes(usage.total)
        };
        parts.push(format!(
            "used {} of {total}",
            crate::util::fmt_bytes(usage.used())
        ));
        if usage.expire > 0 {
            parts.push(format!("expires {}", crate::util::fmt_date(usage.expire)));
        }
    }
    parts.join(", ")
}
