//! Provider metadata carried in response headers (see docs/SUBSCRIPTIONS.md §2.4).

use std::time::Duration;

use base64::Engine as _;
use serde::Serialize;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub upload: u64,
    pub download: u64,
    /// 0 means unlimited.
    pub total: u64,
    /// Unix time; 0 means never.
    pub expire: u64,
}

impl Usage {
    pub fn used(&self) -> u64 {
        self.upload.saturating_add(self.download)
    }

    /// `100.00 GiB`, or `∞` for unlimited plans.
    pub fn total_display(&self) -> String {
        if self.total == 0 {
            "∞".into()
        } else {
            crate::util::fmt_bytes(self.total)
        }
    }

    /// Whole days until `expire` (0 once expired; meaningless when `expire == 0`).
    pub fn days_left(&self, now: u64) -> u64 {
        self.expire.saturating_sub(now) / 86_400
    }
}

/// Remnawave's device-limit signals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct HwidFlags {
    /// `x-hwid-active`: the panel enforces a device limit for this user.
    pub active: bool,
    /// `x-hwid-not-supported`: no valid `x-hwid` header was received.
    pub not_supported: bool,
    /// `x-hwid-max-devices-reached`: this HWID is new and all slots are taken.
    pub max_devices_reached: bool,
    /// `x-hwid-limit`: generic refusal marker (v2RayTun compatibility).
    pub limit: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ProviderInfo {
    pub title: Option<String>,
    pub usage: Option<Usage>,
    pub update_interval: Option<Duration>,
    pub support_url: Option<String>,
    pub web_page_url: Option<String>,
    pub announce: Option<String>,
    /// Unix time of the next traffic reset.
    pub refill_date: Option<u64>,
    pub hwid: HwidFlags,
    /// FlClashX: provider moved to another host.
    pub new_domain: Option<String>,
}

impl ProviderInfo {
    pub fn from_headers(headers: &[(String, String)]) -> Self {
        let get = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.trim())
                .filter(|v| !v.is_empty())
        };
        let flag = |name: &str| get(name).is_some_and(|v| v.eq_ignore_ascii_case("true"));
        Self {
            title: get("profile-title")
                .map(decode_text)
                .or_else(|| get("content-disposition").and_then(disposition_filename))
                .map(|t| crate::util::sanitize(&t)),
            usage: get("subscription-userinfo").map(parse_userinfo),
            // Hours; absurd values would overflow the schedule, so cap at a year.
            update_interval: get("profile-update-interval")
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|&h| h > 0)
                .map(|h| Duration::from_secs(h.min(8760) * 3600)),
            support_url: get("support-url").map(crate::util::sanitize),
            web_page_url: get("profile-web-page-url").map(crate::util::sanitize),
            announce: get("announce").map(|a| crate::util::sanitize(&decode_text(a))),
            refill_date: get("subscription-refill-date").and_then(|v| v.parse().ok()),
            hwid: HwidFlags {
                active: flag("x-hwid-active"),
                not_supported: flag("x-hwid-not-supported"),
                max_devices_reached: flag("x-hwid-max-devices-reached"),
                limit: flag("x-hwid-limit"),
            },
            new_domain: get("flclashx-newdomain").map(str::to_owned),
        }
    }
}

/// `base64:<data>` (Remnawave's `rwEncodeBase64`) or plain text.
pub fn decode_text(value: &str) -> String {
    let Some(encoded) = value.strip_prefix("base64:") else {
        return value.to_owned();
    };
    decode_base64(encoded)
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_else(|| value.to_owned())
}

/// Lenient base64: standard or URL-safe alphabet, padding optional, whitespace ignored.
pub fn decode_base64(input: &str) -> Option<Vec<u8>> {
    use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
    let compact: String = input
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .trim_end_matches('=')
        .to_owned();
    STANDARD_NO_PAD
        .decode(&compact)
        .or_else(|_| URL_SAFE_NO_PAD.decode(&compact))
        .ok()
}

/// `upload=0; download=123; total=456; expire=1767225600` (order and spacing vary).
fn parse_userinfo(value: &str) -> Usage {
    let mut usage = Usage::default();
    for part in value.split(';') {
        let Some((key, raw)) = part.split_once('=') else {
            continue;
        };
        // Some panels send floats ("123.0") or empty values.
        let number = raw
            .trim()
            .split('.')
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        match key.trim().to_ascii_lowercase().as_str() {
            "upload" => usage.upload = number,
            "download" => usage.download = number,
            "total" => usage.total = number,
            "expire" => usage.expire = number,
            _ => {}
        }
    }
    usage
}

/// `attachment; filename*=UTF-8''%D0%9C%D0%BE%D0%B9` or `attachment; filename="name"`.
fn disposition_filename(value: &str) -> Option<String> {
    if let Some(idx) = value.find("filename*=") {
        let encoded = value[idx + 10..].split(';').next()?.trim();
        let encoded = encoded.rsplit("''").next()?;
        return Some(percent_decode(encoded)).filter(|s| !s.is_empty());
    }
    let idx = value.find("filename=")?;
    let name = value[idx + 9..].split(';').next()?.trim().trim_matches('"');
    Some(name.to_owned()).filter(|s| !s.is_empty())
}

pub fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let decoded = (bytes[i] == b'%' && i + 2 < bytes.len())
            .then(|| std::str::from_utf8(&bytes[i + 1..i + 3]).ok())
            .flatten()
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match decoded {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    // Lossy: a stray invalid sequence must not hide the rest of the name.
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn parses_remnawave_headers() {
        let info = ProviderInfo::from_headers(&headers(&[
            (
                "Subscription-Userinfo",
                "upload=0; download=1073741824; total=107374182400; expire=1767225600",
            ),
            ("profile-title", "base64:0JzQvtC5IFZQTg=="),
            ("profile-update-interval", "12"),
            ("support-url", "https://t.me/support"),
            ("announce", "base64:0J/RgNC40LLQtdGC"),
            ("subscription-refill-date", "1767225600"),
            ("x-hwid-active", "true"),
        ]));
        let usage = info.usage.unwrap();
        assert_eq!(usage.used(), 1_073_741_824);
        assert_eq!(usage.total, 107_374_182_400);
        assert_eq!(usage.expire, 1_767_225_600);
        assert_eq!(info.title.as_deref(), Some("Мой VPN"));
        assert_eq!(info.announce.as_deref(), Some("Привет"));
        assert_eq!(info.update_interval, Some(Duration::from_secs(12 * 3600)));
        assert!(info.hwid.active && !info.hwid.max_devices_reached);

        let huge =
            ProviderInfo::from_headers(&headers(&[("profile-update-interval", "99999999999999")]));
        assert_eq!(huge.update_interval, Some(Duration::from_secs(8760 * 3600)));
        assert_eq!(info.refill_date, Some(1_767_225_600));
    }

    #[test]
    fn falls_back_to_disposition() {
        let info = ProviderInfo::from_headers(&headers(&[(
            "content-disposition",
            "attachment; filename*=UTF-8''%D0%9C%D0%BE%D0%B9",
        )]));
        assert_eq!(info.title.as_deref(), Some("Мой"));
        let info = ProviderInfo::from_headers(&headers(&[(
            "Content-Disposition",
            "attachment; filename=\"user42\"",
        )]));
        assert_eq!(info.title.as_deref(), Some("user42"));
    }

    #[test]
    fn tolerates_messy_values() {
        let usage = parse_userinfo("upload=1.0;download=;total=5; expire=0;junk");
        assert_eq!(
            usage,
            Usage {
                upload: 1,
                download: 0,
                total: 5,
                expire: 0
            }
        );
        assert_eq!(decode_text("base64:!!!"), "base64:!!!");
        assert_eq!(decode_text("plain"), "plain");
        assert_eq!(decode_base64("aGk").unwrap(), b"hi");
        assert_eq!(decode_base64("aGk=\n").unwrap(), b"hi");
        assert_eq!(percent_decode("%"), "%");
        assert_eq!(percent_decode("a%2"), "a%2");
        assert_eq!(percent_decode("%zz%41"), "%zzA");
    }
}
