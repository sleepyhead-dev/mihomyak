//! Detects provider "stub" responses: syntactically valid configs whose only
//! purpose is to show a message ("App not supported", "Limit of devices reached",
//! "Subscription expired", …) instead of real servers. Remnawave renders those as
//! `vless` proxies pointing at `0.0.0.0:1` (docs/SUBSCRIPTIONS.md §2.5).

use super::body::{Content, Endpoint};
use super::headers::HwidFlags;

/// Why a response must not be applied, if it is a stub.
pub fn detect(hwid: HwidFlags, content: &Content) -> Option<String> {
    if let Some(reason) = hwid_refusal(hwid) {
        return Some(reason);
    }
    if content.endpoints.is_empty() {
        return (!content.has_providers).then(|| "the subscription contains no proxies".into());
    }
    // Placeholders next to proxy-providers are decoration: the real proxies live in
    // the providers, which we cannot inspect.
    if !content.has_providers && content.endpoints.iter().all(is_placeholder) {
        let names: Vec<&str> = content
            .endpoints
            .iter()
            .map(|e| e.name.as_str())
            .filter(|n| !n.is_empty())
            .collect();
        return Some(format!(
            "every proxy is a placeholder (0.0.0.0/loopback): {}",
            if names.is_empty() {
                "<unnamed>".into()
            } else {
                names.join(" | ")
            }
        ));
    }
    None
}

/// Remnawave marks device-limit refusals with headers even when the body is empty.
pub fn hwid_refusal(hwid: HwidFlags) -> Option<String> {
    if hwid.max_devices_reached {
        Some("device limit reached: this HWID is new and every slot is taken (x-hwid-max-devices-reached)".into())
    } else if hwid.not_supported {
        Some("the panel requires a valid HWID header (x-hwid-not-supported)".into())
    } else if hwid.limit {
        Some("the panel refused this device (x-hwid-limit)".into())
    } else {
        None
    }
}

fn is_placeholder(endpoint: &Endpoint) -> bool {
    let server = endpoint.server.trim().trim_matches(['[', ']']);
    endpoint.port <= 1
        || matches!(server, "" | "0.0.0.0" | "::" | "::1" | "localhost")
        || server.starts_with("127.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subscription::body::Format;

    fn content(endpoints: &[(&str, &str, u16)], has_providers: bool) -> Content {
        Content {
            format: Format::Mihomo,
            yaml: None,
            links: None,
            endpoints: endpoints
                .iter()
                .map(|(n, s, p)| Endpoint {
                    name: n.to_string(),
                    server: s.to_string(),
                    port: *p,
                })
                .collect(),
            has_providers,
            notes: Vec::new(),
        }
    }

    #[test]
    fn detects_remnawave_remarks() {
        let c = content(
            &[
                ("⌛ Subscription expired", "0.0.0.0", 1),
                ("Contact support", "0.0.0.0", 1),
            ],
            false,
        );
        let reason = detect(HwidFlags::default(), &c).unwrap();
        assert!(reason.contains("⌛ Subscription expired | Contact support"));
    }

    #[test]
    fn detects_loopback_and_empty() {
        assert!(
            detect(
                HwidFlags::default(),
                &content(&[("x", "127.0.0.1", 443)], false)
            )
            .is_some()
        );
        assert!(detect(HwidFlags::default(), &content(&[], false)).is_some());
        assert!(detect(HwidFlags::default(), &content(&[], true)).is_none());
        // Remnawave-style info entry next to real proxy-providers is not a stub.
        let info = content(&[("Expires 01.01", "0.0.0.0", 1)], true);
        assert!(detect(HwidFlags::default(), &info).is_none());
    }

    #[test]
    fn accepts_real_servers_even_with_a_placeholder() {
        let c = content(
            &[
                ("Info: renew soon", "0.0.0.0", 1),
                ("NL", "nl.example.com", 443),
            ],
            false,
        );
        assert!(detect(HwidFlags::default(), &c).is_none());
    }

    #[test]
    fn hwid_headers_win() {
        let flags = HwidFlags {
            active: true,
            max_devices_reached: true,
            limit: true,
            ..Default::default()
        };
        let c = content(&[("NL", "nl.example.com", 443)], false);
        assert!(detect(flags, &c).unwrap().contains("device limit"));
        let flags = HwidFlags {
            active: true,
            ..Default::default()
        };
        assert!(detect(flags, &c).is_none());
    }
}
