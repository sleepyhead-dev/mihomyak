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
            config,
            store,
            emulation,
            secret,
            client: http::Client {
                proxy,
                ..http::Client::default()
            },
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

    /// How long to wait after a successful update.
    pub fn interval(&self, info: &ProviderInfo) -> Duration {
        let interval = match self.config.subscription.interval {
            Interval::Fixed(d) => d,
            Interval::Auto => info.update_interval.unwrap_or(DEFAULT_INTERVAL),
        };
        interval.max(MIN_INTERVAL)
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
        };
        self.store.save_meta(&meta)?;
        Ok(Outcome::Applied {
            changed,
            info: analysis.info,
            proxies: meta.proxies,
        })
    }

    /// Rebuilds the config from the cached body (current settings applied).
    /// Returns the time left until the next scheduled update, or `None` when
    /// there is no usable cache for the configured URL and client.
    pub fn restore(&self) -> Result<Option<Duration>> {
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
        // It was accepted when fetched, possibly via accept_stub: don't re-judge it.
        let Some(content) = analysis.content.as_ref() else {
            return Ok(None);
        };
        self.write_config(content)?;
        let due = meta.fetched_at + self.interval(&analysis.info).as_secs();
        Ok(Some(Duration::from_secs(due.saturating_sub(now_unix()))))
    }

    fn write_config(&self, content: &subscription::Content) -> Result<()> {
        let mode = self
            .store
            .mode()
            .unwrap_or_else(|| self.config.core.mode.clone());
        let built = crate::profile::build(content, &self.config, &self.secret, &mode)?;
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
