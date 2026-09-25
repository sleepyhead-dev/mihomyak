//! Byte-exact emulation of real subscription clients.
//!
//! Every constant and formula here was verified against the real application
//! (source code plus a captured request, see `docs/SUBSCRIPTIONS.md` §7).
//! Golden copies of the captured requests live in `tests/fixtures/requests/`.

use anyhow::{Result, bail};
use serde::Deserialize;

use crate::config::Config;
use crate::http::Url;
use crate::identity::Identity;
use crate::util::sha256_hex;

/// FlClashX release tag the defaults emulate (`FlClash X/v<this>`).
pub const FLCLASHX_VERSION: &str = "0.4.2";
/// mihomo version embedded in that FlClashX release (`core/<this>`).
pub const FLCLASHX_CORE_VERSION: &str = "v1.19.28";
/// Koala Clash `package.json` version (`koala-clash/<this>`).
pub const KOALA_VERSION: &str = "1.4.1";

/// Remnawave ≥ 3.0 silently ignores HWIDs that do not match this pattern.
pub fn is_valid_hwid(hwid: &str) -> bool {
    (10..=64).contains(&hwid.len())
        && hwid
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'=' || b == b'-')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub enum ClientKind {
    /// FlClashX (Flutter, dart:io). Default: gets mihomo YAML from every panel.
    FlClashX,
    /// Koala Clash (Electron, axios).
    Koala,
    /// Generic client; requires an explicit User-Agent.
    Custom,
}

impl std::str::FromStr for ClientKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "flclashx" | "flclash-x" | "flclash_x" => Self::FlClashX,
            "koala" | "koala-clash" | "koala_clash" => Self::Koala,
            "custom" => Self::Custom,
            _ => bail!("unknown client {s:?}: expected flclashx, koala or custom"),
        })
    }
}

impl TryFrom<String> for ClientKind {
    type Error = anyhow::Error;

    fn try_from(s: String) -> Result<Self> {
        s.parse()
    }
}

impl std::fmt::Display for ClientKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::FlClashX => "flclashx",
            Self::Koala => "koala",
            Self::Custom => "custom",
        })
    }
}

/// Values of the Remnawave-style device headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceHeaders {
    pub hwid: String,
    pub os: String,
    pub os_version: Option<String>,
    pub model: String,
}

#[derive(Debug, Clone)]
pub struct Emulation {
    pub kind: ClientKind,
    app_version: String,
    core_version: String,
    user_agent: Option<String>,
    extra_headers: Vec<(String, String)>,
    hwid_override: Option<String>,
    send_device_headers: bool,
    identity: Identity,
}

impl Emulation {
    pub fn new(config: &Config, identity: Identity) -> Result<Self> {
        let sub = &config.subscription;
        if sub.client == ClientKind::Custom && sub.user_agent.is_none() {
            bail!("client \"custom\" needs subscription.user_agent (MIHOMYAK_USER_AGENT)");
        }
        let default_version = match sub.client {
            ClientKind::Koala => KOALA_VERSION,
            _ => FLCLASHX_VERSION,
        };
        let extra_headers = sub
            .headers
            .iter()
            .filter_map(|h| h.split_once(':'))
            .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
            .collect();
        let emulation = Self {
            kind: sub.client,
            app_version: sub
                .app_version
                .clone()
                .unwrap_or_else(|| default_version.into()),
            core_version: sub
                .core_version
                .clone()
                .unwrap_or_else(|| FLCLASHX_CORE_VERSION.into()),
            user_agent: sub.user_agent.clone(),
            extra_headers,
            hwid_override: config.device.hwid.clone(),
            send_device_headers: config.device.send_headers,
            identity,
        };
        let hwid = emulation.device_headers().hwid;
        if emulation.send_device_headers && !is_valid_hwid(&hwid) {
            crate::warn!(
                "HWID {hwid:?} does not match ^[a-zA-Z0-9=-]{{10,64}}$; Remnawave will treat it as missing"
            );
        }
        Ok(emulation)
    }

    pub fn user_agent(&self) -> String {
        if let Some(ua) = &self.user_agent {
            return ua.clone();
        }
        match self.kind {
            // lib/common/package.dart: [FlClash X/v$ver, core/$core, Platform/$os].join(" ")
            ClientKind::FlClashX => format!(
                "FlClash X/v{} core/{} Platform/linux",
                self.app_version, self.core_version
            ),
            // src/main/utils/userAgent.ts
            ClientKind::Koala => format!("koala-clash/{}", self.app_version),
            ClientKind::Custom => unreachable!("validated in Emulation::new"),
        }
    }

    pub fn device_headers(&self) -> DeviceHeaders {
        let id = &self.identity;
        let digest = sha256_hex(id.machine_id.as_bytes());
        match self.kind {
            // lib/utils/device_info_service.dart (Linux branch)
            ClientKind::FlClashX => DeviceHeaders {
                hwid: self
                    .hwid_override
                    .clone()
                    .unwrap_or_else(|| digest[..16].to_ascii_uppercase()),
                os: "Linux".into(),
                os_version: id.os.get("VERSION_ID"),
                model: id.os.get("NAME").unwrap_or_else(|| "Linux".into()),
            },
            // src/main/utils/deviceInfo.ts (linux branches)
            ClientKind::Koala | ClientKind::Custom => {
                let name = id.os.koala_get("NAME");
                let version = id.os.koala_get("VERSION_ID");
                let os_version = match (&name, &version) {
                    (Some(n), Some(v)) => format!("{n} {v}"),
                    (Some(n), None) => n.clone(),
                    _ => id.kernel_release.clone(),
                };
                DeviceHeaders {
                    hwid: self
                        .hwid_override
                        .clone()
                        .unwrap_or_else(|| digest[..16].to_owned()),
                    os: "Linux".into(),
                    os_version: Some(os_version),
                    model: id
                        .os
                        .koala_get("PRETTY_NAME")
                        .or(name)
                        .unwrap_or_else(|| "Linux".into()),
                }
            }
        }
    }

    /// The complete, ordered request header list for a subscription fetch.
    pub fn headers(&self, url: &Url) -> Vec<(String, String)> {
        let host = url.host_header();
        let ua = self.user_agent();
        let dev = self.send_device_headers.then(|| self.device_headers());
        let mut headers: Vec<(&str, String)> = Vec::with_capacity(10);
        match self.kind {
            ClientKind::FlClashX => {
                // dart:io lower-cases names; order is its internal HashMap iteration,
                // stable for this key set (captured from the real app).
                headers.push(("user-agent", ua));
                if let Some(d) = &dev {
                    headers.push(("x-device-model", d.model.clone()));
                    if let Some(v) = &d.os_version {
                        headers.push(("x-ver-os", v.clone()));
                    }
                }
                headers.push(("accept-encoding", "gzip".into()));
                headers.push(("host", host));
                if let Some(d) = &dev {
                    headers.push(("x-device-os", d.os.clone()));
                    headers.push(("x-hwid", d.hwid.clone()));
                }
            }
            ClientKind::Koala => {
                // axios 1.x on Node 22: defaults, user headers, then http module's.
                headers.push(("Accept", "application/json, text/plain, */*".into()));
                headers.push(("User-Agent", ua));
                if let Some(d) = &dev {
                    headers.push(("x-hwid", d.hwid.clone()));
                    headers.push(("x-device-os", d.os.clone()));
                    headers.push(("x-ver-os", d.os_version.clone().unwrap_or_default()));
                    headers.push(("x-device-model", d.model.clone()));
                }
                headers.push(("Accept-Encoding", "gzip, compress, deflate, br".into()));
                headers.push(("Host", host));
                headers.push(("Connection", "keep-alive".into()));
            }
            ClientKind::Custom => {
                headers.push(("Host", host));
                headers.push(("User-Agent", ua));
                headers.push(("Accept", "*/*".into()));
                headers.push(("Accept-Encoding", "gzip, deflate, br".into()));
                if let Some(d) = &dev {
                    headers.push(("x-hwid", d.hwid.clone()));
                    headers.push(("x-device-os", d.os.clone()));
                    if let Some(v) = &d.os_version {
                        headers.push(("x-ver-os", v.clone()));
                    }
                    headers.push(("x-device-model", d.model.clone()));
                }
                headers.push(("Connection", "close".into()));
            }
        }
        let mut out: Vec<(String, String)> = headers
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
        for (name, value) in &self.extra_headers {
            match out.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(name)) {
                Some(slot) => *slot = (name.clone(), value.clone()),
                None => out.push((name.clone(), value.clone())),
            }
        }
        out
    }

    /// Whether the emulated client honours the `flclashx-newdomain` header.
    pub fn follows_new_domain(&self) -> bool {
        self.kind == ClientKind::FlClashX
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::OsRelease;

    const UBUNTU: &str =
        "PRETTY_NAME=\"Ubuntu 24.04.3 LTS\"\nNAME=\"Ubuntu\"\nVERSION_ID=\"24.04\"\n";

    fn emulation(kind: ClientKind, os: &str) -> Emulation {
        let mut config = Config::default();
        config.subscription.client = kind;
        if kind == ClientKind::Custom {
            config.subscription.user_agent = Some("clash-verge/v2.4.0".into());
        }
        let identity = Identity {
            machine_id: "0d0af05ee8fd4dc29275718f2ce4dff1".into(),
            os: OsRelease::from_raw(os),
            kernel_release: "6.8.0-45-generic".into(),
        };
        Emulation::new(&config, identity).unwrap()
    }

    fn render(e: &Emulation, url: &str) -> String {
        e.headers(&Url::parse(url).unwrap())
            .iter()
            .map(|(k, v)| format!("{k}: {v}\n"))
            .collect()
    }

    #[test]
    fn flclashx_matches_capture() {
        let e = emulation(ClientKind::FlClashX, UBUNTU);
        assert_eq!(
            render(&e, "http://127.0.0.1:18082/sub/abc"),
            "user-agent: FlClash X/v0.4.2 core/v1.19.28 Platform/linux\n\
             x-device-model: Ubuntu\n\
             x-ver-os: 24.04\n\
             accept-encoding: gzip\n\
             host: 127.0.0.1:18082\n\
             x-device-os: Linux\n\
             x-hwid: A3B522EAA6F7DD89\n"
        );
    }

    #[test]
    fn koala_matches_capture() {
        let e = emulation(ClientKind::Koala, UBUNTU);
        assert_eq!(
            render(&e, "http://127.0.0.1:18081/sub/abc"),
            "Accept: application/json, text/plain, */*\n\
             User-Agent: koala-clash/1.4.1\n\
             x-hwid: a3b522eaa6f7dd89\n\
             x-device-os: Linux\n\
             x-ver-os: Ubuntu 24.04\n\
             x-device-model: Ubuntu 24.04.3 LTS\n\
             Accept-Encoding: gzip, compress, deflate, br\n\
             Host: 127.0.0.1:18081\n\
             Connection: keep-alive\n"
        );
    }

    #[test]
    fn distro_without_version_id() {
        // Arch Linux ships no VERSION_ID.
        let arch = "NAME=\"Arch Linux\"\nPRETTY_NAME=\"Arch Linux\"\nID=arch\n";
        let flx = emulation(ClientKind::FlClashX, arch).device_headers();
        assert_eq!(flx.os_version, None);
        assert_eq!(flx.model, "Arch Linux");
        let koala = emulation(ClientKind::Koala, arch).device_headers();
        assert_eq!(koala.os_version.as_deref(), Some("Arch Linux"));

        let none = emulation(ClientKind::Koala, "").device_headers();
        assert_eq!(none.os_version.as_deref(), Some("6.8.0-45-generic"));
        assert_eq!(none.model, "Linux");
        assert_eq!(
            emulation(ClientKind::FlClashX, "").device_headers().model,
            "Linux"
        );
    }

    #[test]
    fn overrides_apply_in_place() {
        let mut config = Config::default();
        config.subscription.user_agent = Some("FlClash X/v9.9.9".into());
        config.subscription.headers = vec!["Accept-Encoding: identity".into(), "X-New: 1".into()];
        config.device.hwid = Some("CUSTOM-HWID-123".into());
        let identity = Identity {
            machine_id: "x".into(),
            os: OsRelease::from_raw(UBUNTU),
            kernel_release: String::new(),
        };
        let e = Emulation::new(&config, identity).unwrap();
        let h = e.headers(&Url::parse("https://s.example/a").unwrap());
        assert_eq!(h[0], ("user-agent".into(), "FlClash X/v9.9.9".into()));
        assert_eq!(h[3], ("Accept-Encoding".into(), "identity".into()));
        assert_eq!(h[4], ("host".into(), "s.example".into()));
        assert_eq!(h[6], ("x-hwid".into(), "CUSTOM-HWID-123".into()));
        assert_eq!(h.last().unwrap(), &("X-New".into(), "1".into()));
    }

    #[test]
    fn device_headers_can_be_disabled() {
        let mut config = Config::default();
        config.device.send_headers = false;
        let identity = Identity {
            machine_id: "x".into(),
            os: OsRelease::default(),
            kernel_release: String::new(),
        };
        let e = Emulation::new(&config, identity).unwrap();
        let h = e.headers(&Url::parse("https://s.example/a").unwrap());
        assert!(h.iter().all(|(k, _)| !k.starts_with("x-")));
    }

    #[test]
    fn custom_client_needs_user_agent() {
        let mut config = Config::default();
        config.subscription.client = ClientKind::Custom;
        let identity = Identity {
            machine_id: "x".into(),
            os: OsRelease::default(),
            kernel_release: String::new(),
        };
        assert!(Emulation::new(&config, identity).is_err());
        let e = emulation(ClientKind::Custom, UBUNTU);
        assert_eq!(e.user_agent(), "clash-verge/v2.4.0");
    }

    #[test]
    fn validates_hwid_like_remnawave() {
        assert!(is_valid_hwid("A3B522EAA6F7DD89"));
        assert!(is_valid_hwid("0d0af05ee8fd4dc29275718f2ce4dff1"));
        assert!(!is_valid_hwid("short"));
        assert!(!is_valid_hwid("has_underscore_1234"));
        assert!(!is_valid_hwid(&"a".repeat(65)));
    }

    #[test]
    fn parses_client_names() {
        assert_eq!(
            "FlClashX".parse::<ClientKind>().unwrap(),
            ClientKind::FlClashX
        );
        assert_eq!(
            "koala-clash".parse::<ClientKind>().unwrap(),
            ClientKind::Koala
        );
        assert!("happ".parse::<ClientKind>().is_err());
    }
}
