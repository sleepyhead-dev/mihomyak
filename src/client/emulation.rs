//! Byte-exact emulation of real subscription clients.
//!
//! Every constant and formula here was verified against the real application
//! (source code plus a captured request, see `docs/dev/SUBSCRIPTIONS.md` §7).
//! Golden copies of the captured requests live in `tests/fixtures/requests/`.

use anyhow::{Result, bail};
use serde::Deserialize;

use crate::client::http::Url;
use crate::client::identity::Identity;
use crate::config::Config;
use crate::util::sha256_hex;

/// FlClashX release tag the defaults emulate (`FlClash X/v<this>`).
pub const FLCLASHX_VERSION: &str = "0.4.2";
/// mihomo version embedded in that FlClashX release (`core/<this>`).
pub const FLCLASHX_CORE_VERSION: &str = "v1.19.28";
/// Koala Clash `package.json` version (`koala-clash/<this>`).
pub const KOALA_VERSION: &str = "1.4.1";
/// Happ Desktop for Linux release (`Happ/<this>/Linux/…`, `X-App-Version`).
pub const HAPP_VERSION: &str = "4.3.0";
/// Build ids compiled into the Happ 4.3.0 Linux x64 / arm64 binaries.
pub const HAPP_BUILD_X64: &str = "2609151457";
pub const HAPP_BUILD_ARM64: &str = "2609151456";

/// Happ's rolling User-Agent marker, reverse-engineered from the 4.3.0 binary:
/// `(QDateTime::currentDateTimeUtc().addSecs(10800).date().day() & 1) ? '5' : '6'`,
/// i.e. it flips daily with the day of month in Moscow time (UTC+3).
pub fn happ_day_marker(unix: u64) -> char {
    if crate::util::day_of_month(unix + 3 * 3600) % 2 == 1 {
        '5'
    } else {
        '6'
    }
}

/// `QSysInfo::currentCpuArchitecture()` for this machine.
fn qt_cpu_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86" => "i386",
        other => other,
    }
}

/// Happ ships only x64 and arm64 Linux builds.
fn happ_default_build() -> &'static str {
    if std::env::consts::ARCH == "aarch64" {
        HAPP_BUILD_ARM64
    } else {
        HAPP_BUILD_X64
    }
}

/// (`X-Device-Locale`, `Accept-Language`) as Happ/Qt produce them for a POSIX
/// locale. Qt sends `QLocale::system().name()` with `-`, followed by `,*` for
/// English and `,en,*` otherwise; the C locale counts as `en`.
/// Verified: `C`/`en` → (`EN`, `en,*`), `ru_RU` → (`RU`, `ru-RU,en,*`).
/// A language without a region gets Qt's likely region (CLDR likely subtags);
/// that part is derived from Qt's rules, not captured.
fn happ_locale(locale: &str) -> (String, String) {
    let locale = locale.split(['.', '@']).next().unwrap_or_default();
    let (lang, region) = match locale.split_once(['_', '-']) {
        Some((l, r)) => (l.to_ascii_lowercase(), Some(r.to_ascii_uppercase())),
        None => (locale.to_ascii_lowercase(), None),
    };
    if matches!(lang.as_str(), "" | "c" | "posix" | "en") && region.is_none() {
        return ("EN".into(), "en,*".into());
    }
    let region = region.or_else(|| likely_region(&lang).map(str::to_owned));
    let name = match region {
        Some(region) => format!("{lang}-{region}"),
        None => lang.clone(),
    };
    let tail = if lang == "en" { ",*" } else { ",en,*" };
    (lang.to_ascii_uppercase(), format!("{name}{tail}"))
}

/// CLDR likely regions for the languages Happ users typically run.
fn likely_region(lang: &str) -> Option<&'static str> {
    Some(match lang {
        "ru" => "RU",
        "uk" => "UA",
        "be" => "BY",
        "kk" => "KZ",
        "uz" => "UZ",
        "ky" => "KG",
        "tg" => "TJ",
        "hy" => "AM",
        "ka" => "GE",
        "az" => "AZ",
        "tk" => "TM",
        "fa" => "IR",
        "tr" => "TR",
        "de" => "DE",
        "fr" => "FR",
        "es" => "ES",
        "it" => "IT",
        "pl" => "PL",
        "pt" => "BR",
        "zh" => "CN",
        "ja" => "JP",
        "ko" => "KR",
        "ar" => "EG",
        _ => return None,
    })
}

/// Iteration order of a Dart VM `HashMap<String, …>` after inserting `keys` in
/// order. dart:io keeps request headers in such a map, so this is the wire order
/// of FlClashX's headers. Model of `_HashMap` (sdk/lib/_internal/vm/lib/
/// collection_patch.dart): 8 initial buckets, new entries prepended to their
/// chain, doubling when `4 * count > 3 * buckets` with chains re-prepended in
/// bucket order; iteration walks buckets in index order.
fn dart_hashmap_order<'a>(keys: &[&'a str]) -> Vec<&'a str> {
    let mut buckets: Vec<Vec<&'a str>> = vec![Vec::new(); 8];
    let mut count = 0usize;
    for &key in keys {
        let len = buckets.len();
        let index = dart_string_hash(key) as usize & (len - 1);
        if buckets[index].contains(&key) {
            continue;
        }
        // Chains are stored head-last so "prepend" is a push.
        buckets[index].push(key);
        count += 1;
        if count * 4 > len * 3 {
            let mut grown: Vec<Vec<&'a str>> = vec![Vec::new(); len * 2];
            for chain in &buckets {
                for &entry in chain.iter().rev() {
                    grown[dart_string_hash(entry) as usize & (len * 2 - 1)].push(entry);
                }
            }
            buckets = grown;
        }
    }
    buckets
        .iter()
        .flat_map(|chain| chain.iter().rev().copied())
        .collect()
}

/// Dart VM `String.hashCode`: Jenkins one-at-a-time over UTF-16 code units,
/// truncated to 30 bits, never 0.
fn dart_string_hash(s: &str) -> u32 {
    let mut h: u32 = 0;
    for unit in s.encode_utf16() {
        h = h.wrapping_add(u32::from(unit));
        h = h.wrapping_add(h << 10);
        h ^= h >> 6;
    }
    h = h.wrapping_add(h << 3);
    h ^= h >> 11;
    h = h.wrapping_add(h << 15);
    match h & ((1 << 30) - 1) {
        0 => 1,
        h => h,
    }
}

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
    /// Happ Desktop (Qt, xray core): panels answer with share links or Xray JSON.
    Happ,
    /// Generic client; requires an explicit User-Agent.
    Custom,
}

impl std::str::FromStr for ClientKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "flclashx" | "flclash-x" | "flclash_x" => Self::FlClashX,
            "koala" | "koala-clash" | "koala_clash" => Self::Koala,
            "happ" => Self::Happ,
            "custom" => Self::Custom,
            _ => bail!("unknown client {s:?}: expected flclashx, koala, happ or custom"),
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
            Self::Happ => "happ",
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
    app_build: String,
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
            ClientKind::Happ => HAPP_VERSION,
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
            app_build: sub
                .app_build
                .clone()
                .unwrap_or_else(|| happ_default_build().into()),
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
        self.user_agent_at(crate::util::now_unix())
    }

    /// User-Agent at a given time (Happ's changes daily).
    pub fn user_agent_at(&self, unix: u64) -> String {
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
            // QString("Happ/%1/%2/%3%4%5").arg(version, "Linux", build, marker, "98")
            ClientKind::Happ => format!(
                "Happ/{}/Linux/{}{}98",
                self.app_version,
                self.app_build,
                happ_day_marker(unix)
            ),
            ClientKind::Custom => unreachable!("validated in Emulation::new"),
        }
    }

    pub fn device_headers(&self) -> DeviceHeaders {
        let id = &self.identity;
        let digest = sha256_hex(id.machine_id.as_bytes());
        match self.kind {
            // lib/utils/device_info_service.dart (Linux branch)
            // device_info_plus: versionId = VERSION_ID ?? DISTRIB_RELEASE (an empty
            // value is still sent), name = NAME ?? "Linux".
            ClientKind::FlClashX => DeviceHeaders {
                hwid: self
                    .hwid_override
                    .clone()
                    .unwrap_or_else(|| digest[..16].to_ascii_uppercase()),
                os: "Linux".into(),
                os_version: id
                    .os
                    .dip_get("VERSION_ID")
                    .or_else(|| id.os.dip_lsb("DISTRIB_RELEASE")),
                model: id.os.dip_get("NAME").unwrap_or_else(|| "Linux".into()),
            },
            // Captured from Happ 4.3.0: QSysInfo machineUniqueId / hostname_arch /
            // productType_productVersion.
            ClientKind::Happ => DeviceHeaders {
                hwid: self
                    .hwid_override
                    .clone()
                    .unwrap_or_else(|| id.machine_id.clone()),
                os: "Linux".into(),
                os_version: Some(format!(
                    "{}_{}",
                    id.os.get("ID").unwrap_or_else(|| "unknown".into()),
                    id.os.get("VERSION_ID").unwrap_or_else(|| "unknown".into())
                )),
                model: format!("{}_{}", id.hostname, qt_cpu_arch()),
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
    /// Control characters (a CRLF-terminated os-release, an odd hostname) are
    /// dropped from values: they would corrupt or inject request headers.
    pub fn headers(&self, url: &Url) -> Vec<(String, String)> {
        let mut headers = self.build_headers(url);
        for (_, value) in &mut headers {
            value.retain(|c| !c.is_control());
        }
        headers
    }

    fn build_headers(&self, url: &Url) -> Vec<(String, String)> {
        let host = url.host_header();
        let ua = self.user_agent();
        let dev = self.send_device_headers.then(|| self.device_headers());
        let mut headers: Vec<(&str, String)> = Vec::with_capacity(10);
        match self.kind {
            ClientKind::FlClashX => return self.flclashx_headers(host, ua, dev),
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
            ClientKind::Happ => {
                // Qt QNetworkAccessManager: Host first, app headers in the order Happ
                // sets them, then Qt's own Connection/Accept-Encoding/Accept-Language.
                let (locale, accept_language) = happ_locale(&self.identity.locale);
                headers.push(("Host", host));
                headers.push(("User-Agent", ua));
                headers.push(("X-App-Version", self.app_version.clone()));
                headers.push(("X-Device-Locale", locale));
                if let Some(d) = &dev {
                    headers.push(("X-Device-Os", d.os.clone()));
                    headers.push(("X-Device-Model", d.model.clone()));
                    headers.push(("X-Hwid", d.hwid.clone()));
                    headers.push(("X-Ver-Os", d.os_version.clone().unwrap_or_default()));
                }
                headers.push(("Connection", "Keep-Alive".into()));
                headers.push(("Accept-Encoding", "zstd, br, gzip, deflate".into()));
                headers.push(("Accept-Language", accept_language));
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

    /// dart:io lower-cases header names and writes them in the iteration order of
    /// its internal `HashMap`, so the order depends on the whole key set (extra
    /// headers included). Insertion order: HttpClient sets `host` and
    /// `accept-encoding`, then FlClashX adds `user-agent` and the device headers.
    fn flclashx_headers(
        &self,
        host: String,
        ua: String,
        dev: Option<DeviceHeaders>,
    ) -> Vec<(String, String)> {
        let mut values: Vec<(String, String)> = vec![
            ("host".into(), host),
            ("accept-encoding".into(), "gzip".into()),
            ("user-agent".into(), ua),
        ];
        if let Some(d) = dev {
            values.push(("x-hwid".into(), d.hwid));
            values.push(("x-device-os".into(), d.os));
            values.push(("x-device-model".into(), d.model));
            if let Some(v) = d.os_version {
                values.push(("x-ver-os".into(), v));
            }
        }
        for (name, value) in &self.extra_headers {
            let name = name.to_ascii_lowercase();
            match values.iter_mut().find(|(k, _)| *k == name) {
                Some(slot) => slot.1 = value.clone(),
                None => values.push((name, value.clone())),
            }
        }
        let keys: Vec<&str> = values.iter().map(|(k, _)| k.as_str()).collect();
        dart_hashmap_order(&keys)
            .into_iter()
            .filter_map(|key| values.iter().find(|(k, _)| k == key).cloned())
            .collect()
    }

    /// Whether the emulated client honours the `flclashx-newdomain` header.
    pub fn follows_new_domain(&self) -> bool {
        self.kind == ClientKind::FlClashX
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::identity::OsRelease;

    const UBUNTU: &str =
        "PRETTY_NAME=\"Ubuntu 24.04.3 LTS\"\nNAME=\"Ubuntu\"\nVERSION_ID=\"24.04\"\nID=ubuntu\n";

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
            hostname: "vm".into(),
            locale: "en".into(),
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
    fn dart_hashmap_model_reproduces_captures() {
        // The 7-key order captured from FlClashX 0.4.2 (resize to 16 buckets).
        let seven = [
            "host",
            "accept-encoding",
            "user-agent",
            "x-hwid",
            "x-device-os",
            "x-device-model",
            "x-ver-os",
        ];
        assert_eq!(
            dart_hashmap_order(&seven),
            [
                "user-agent",
                "x-device-model",
                "x-ver-os",
                "accept-encoding",
                "host",
                "x-device-os",
                "x-hwid"
            ]
        );
        // Without x-ver-os the map never resizes and the order changes.
        assert_eq!(
            dart_hashmap_order(&seven[..6]),
            [
                "user-agent",
                "x-device-model",
                "accept-encoding",
                "x-hwid",
                "x-device-os",
                "host"
            ]
        );
        assert_eq!(
            dart_hashmap_order(&seven[..3]),
            ["user-agent", "accept-encoding", "host"]
        );
        assert_eq!(dart_string_hash(""), 1);
    }

    #[test]
    fn flclashx_order_follows_the_key_set() {
        let arch = "NAME=\"Arch Linux\"\nID=arch\n";
        let e = emulation(ClientKind::FlClashX, arch);
        let names: Vec<String> = e
            .headers(&Url::parse("https://s.example/a").unwrap())
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(
            names,
            [
                "user-agent",
                "x-device-model",
                "accept-encoding",
                "x-hwid",
                "x-device-os",
                "host"
            ]
        );
    }

    #[test]
    fn flclashx_reads_os_release_like_device_info_plus() {
        let os = OsRelease::from_raw("NAME=\"Debian GNU/Linux\"\nVERSION_ID=\n")
            .with_lsb("DISTRIB_RELEASE=12\n");
        let identity = Identity {
            machine_id: "0d0af05ee8fd4dc29275718f2ce4dff1".into(),
            os,
            kernel_release: String::new(),
            hostname: "vm".into(),
            locale: "en".into(),
        };
        let e = Emulation::new(&Config::default(), identity).unwrap();
        let d = e.device_headers();
        // An empty VERSION_ID is a value, so lsb-release is not consulted.
        assert_eq!(d.os_version.as_deref(), Some(""));
        assert_eq!(d.model, "Debian GNU/Linux");

        let lsb_only = OsRelease::from_raw("NAME=Alpine\n").with_lsb("DISTRIB_RELEASE=3.22\n");
        let identity = Identity {
            machine_id: "x".into(),
            os: lsb_only,
            kernel_release: String::new(),
            hostname: "vm".into(),
            locale: "en".into(),
        };
        let e = Emulation::new(&Config::default(), identity).unwrap();
        assert_eq!(e.device_headers().os_version.as_deref(), Some("3.22"));
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
            hostname: "vm".into(),
            locale: "en".into(),
        };
        let e = Emulation::new(&config, identity).unwrap();
        let h = e.headers(&Url::parse("https://s.example/a").unwrap());
        let get = |name: &str| h.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
        assert_eq!(get("user-agent"), Some("FlClash X/v9.9.9"));
        // dart:io lower-cases names; overrides keep their slot, new keys join the map.
        assert_eq!(get("accept-encoding"), Some("identity"));
        assert_eq!(get("x-hwid"), Some("CUSTOM-HWID-123"));
        assert_eq!(get("x-new"), Some("1"));
        assert_eq!(h.len(), 8);
        let mut keys: Vec<&str> = h.iter().map(|(k, _)| k.as_str()).collect();
        let order = dart_hashmap_order(&[
            "host",
            "accept-encoding",
            "user-agent",
            "x-hwid",
            "x-device-os",
            "x-device-model",
            "x-ver-os",
            "x-new",
        ]);
        assert_eq!(keys, order);
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), 8);
    }

    #[test]
    fn device_headers_can_be_disabled() {
        let mut config = Config::default();
        config.device.send_headers = false;
        let identity = Identity {
            machine_id: "x".into(),
            os: OsRelease::default(),
            kernel_release: String::new(),
            hostname: "vm".into(),
            locale: "en".into(),
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
            hostname: "vm".into(),
            locale: "en".into(),
        };
        assert!(Emulation::new(&config, identity).is_err());
        let e = emulation(ClientKind::Custom, UBUNTU);
        assert_eq!(e.user_agent(), "clash-verge/v2.4.0");
    }

    #[test]
    fn happ_matches_capture() {
        let e = emulation(ClientKind::Happ, UBUNTU);
        // 2026-09-25T21:20:27Z is the 26th in Moscow: even day → '6'.
        let ua = e.user_agent_at(1_790_371_227);
        assert_eq!(ua, format!("Happ/4.3.0/Linux/{}698", happ_default_build()));
        let d = e.device_headers();
        assert_eq!(d.hwid, "0d0af05ee8fd4dc29275718f2ce4dff1");
        assert_eq!(d.os_version.as_deref(), Some("ubuntu_24.04"));
        assert_eq!(d.model, format!("vm_{}", qt_cpu_arch()));
        let names: Vec<String> = e
            .headers(&Url::parse("https://s.example/sub").unwrap())
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(
            names,
            [
                "Host",
                "User-Agent",
                "X-App-Version",
                "X-Device-Locale",
                "X-Device-Os",
                "X-Device-Model",
                "X-Hwid",
                "X-Ver-Os",
                "Connection",
                "Accept-Encoding",
                "Accept-Language"
            ]
        );
    }

    #[test]
    fn happ_marker_flips_daily_in_moscow_time() {
        assert_eq!(happ_day_marker(1_790_371_227), '6'); // 26th MSK
        assert_eq!(happ_day_marker(1_790_371_227 - 86_400), '5'); // 25th MSK
        // 22:00 UTC on the 25th is already the 26th in Moscow.
        assert_eq!(happ_day_marker(1_790_373_600), '6');
        assert_eq!(happ_day_marker(1_790_373_600 - 3600 * 2), '5');
    }

    #[test]
    fn happ_locales() {
        assert_eq!(happ_locale("en"), ("EN".into(), "en,*".into()));
        assert_eq!(happ_locale("C.UTF-8"), ("EN".into(), "en,*".into()));
        assert_eq!(
            happ_locale("ru_RU.UTF-8"),
            ("RU".into(), "ru-RU,en,*".into())
        );
        assert_eq!(happ_locale("uk"), ("UK".into(), "uk-UA,en,*".into()));
        assert_eq!(happ_locale("en_GB.UTF-8"), ("EN".into(), "en-GB,*".into()));
        assert_eq!(happ_locale("eo"), ("EO".into(), "eo,en,*".into()));
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
        assert_eq!("Happ".parse::<ClientKind>().unwrap(), ClientKind::Happ);
        assert!("v2rayng".parse::<ClientKind>().is_err());
    }
}
