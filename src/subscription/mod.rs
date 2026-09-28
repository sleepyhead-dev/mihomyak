//! Fetching and understanding a subscription.

pub mod body;
pub mod headers;
pub mod stub;
pub mod xray;

use std::net::IpAddr;

use anyhow::{Context, Result, bail};

pub use body::{Content, Format};
pub use headers::ProviderInfo;

use crate::client::emulation::Emulation;
use crate::client::http::{Client, Endpoint, Request, Response, Scheme, Url};

/// Every emulated client follows redirects; FlClashX allows at most 5.
pub const MAX_REDIRECTS: usize = 5;

pub struct Fetch {
    /// URL that produced the final response.
    pub url: Url,
    /// Headers sent on the final request, in wire order.
    pub request_headers: Vec<(String, String)>,
    pub response: Response,
    /// URLs that answered with a redirect.
    pub hops: Vec<Url>,
    /// Addresses of every server that answered (direct connections only).
    pub peers: Vec<IpAddr>,
}

impl Fetch {
    /// Every host contacted: the configured one, redirect targets, the final one.
    pub fn hosts(&self) -> Vec<String> {
        let mut hosts: Vec<String> = Vec::new();
        for url in self.hops.iter().chain([&self.url]) {
            if !hosts.contains(&url.host) {
                hosts.push(url.host.clone());
            }
        }
        hosts
    }
}

/// GETs the subscription exactly like the emulated client, following redirects
/// with the same header set (Host updated per hop). A redirect from https to plain
/// http is refused: it would send the token and HWID in clear text.
pub fn fetch(client: &Client, emulation: &Emulation, url: &Url) -> Result<Fetch> {
    let mut url = url.clone();
    let mut hops = Vec::new();
    let mut peers = Vec::new();
    loop {
        let headers = emulation.headers(&url);
        let request = Request {
            method: "GET",
            target: &url.target,
            headers: &headers,
            body: &[],
        };
        let response = client
            .send(&Endpoint::from(&url), &request)
            .with_context(|| format!("GET {}", redact(&url)))?;
        if let Some(peer) = response.peer
            && !peers.contains(&peer)
        {
            peers.push(peer);
        }
        if (300..400).contains(&response.status)
            && let Some(location) = response.header("location")
        {
            if hops.len() >= MAX_REDIRECTS {
                bail!("too many redirects (> {MAX_REDIRECTS})");
            }
            let next = url.join(location).context("bad redirect Location")?;
            if url.scheme == Scheme::Https && next.scheme == Scheme::Http {
                bail!("refusing a redirect from https to http ({})", redact(&next));
            }
            hops.push(std::mem::replace(&mut url, next));
            continue;
        }
        return Ok(Fetch {
            url,
            request_headers: headers,
            response,
            hops,
            peers,
        });
    }
}

/// Why a response must not replace the last known good config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// The panel refused this device (HWID headers).
    Refused(String),
    /// Non-2xx status.
    Http(String),
    /// Body is not a usable config.
    Invalid(String),
    /// Valid config that only carries a provider message.
    Stub(String),
}

impl Problem {
    pub fn message(&self) -> &str {
        match self {
            Problem::Refused(m) | Problem::Http(m) | Problem::Invalid(m) | Problem::Stub(m) => m,
        }
    }
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

pub struct Analysis {
    pub info: ProviderInfo,
    /// Parsed body, when it is a config at all (stubs included).
    pub content: Option<Content>,
    pub problem: Option<Problem>,
}

impl Analysis {
    /// Content that may be applied: refused whenever the response carries a
    /// problem, a provider stub included.
    pub fn usable(&self) -> Option<&Content> {
        match &self.problem {
            None => self.content.as_ref(),
            Some(_) => None,
        }
    }
}

/// Classifies a response. `strict_content_type` rejects `Content-Type: text/html`
/// regardless of the body (Koala does; FlClashX and Happ only look at the body).
pub fn analyze(response: &Response, strict_content_type: bool) -> Analysis {
    let info = ProviderInfo::from_headers(&response.headers);
    let content_type = response
        .header("content-type")
        .filter(|_| strict_content_type);
    let parsed = body::parse(&response.body, content_type);
    let refusal = stub::hwid_refusal(info.hwid);
    let problem = if let Some(reason) = refusal {
        Some(Problem::Refused(reason))
    } else if !(200..300).contains(&response.status) {
        Some(Problem::Http(http_problem(response)))
    } else {
        match &parsed {
            Err(e) => Some(Problem::Invalid(e.clone())),
            Ok(content) => stub::detect(info.hwid, content).map(Problem::Stub),
        }
    };
    Analysis {
        info,
        content: parsed.ok(),
        problem,
    }
}

fn http_problem(response: &Response) -> String {
    let hint = match response.status {
        403 => ": the panel blocks this client (User-Agent rule) or requires an HWID",
        404 => ": unknown subscription, or the panel requires an HWID",
        429 => ": rate limited by the panel",
        451 => ": the panel's response rules reject this client",
        _ => "",
    };
    format!(
        "HTTP {} {}{hint}",
        response.status,
        crate::util::sanitize(&response.reason)
    )
}

/// Subscription URLs are credentials: keep the host, mask the token.
pub fn redact(url: &Url) -> String {
    let path = url.target.split('?').next().unwrap_or_default();
    let tail: String = path
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{}://{}/…{tail}", url.scheme.as_str(), url.host_header())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(status: u16, headers: &[(&str, &str)], body: &str) -> Response {
        Response {
            status,
            reason: "R".into(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: body.as_bytes().to_vec(),
            peer: None,
        }
    }

    fn analyze_(response: &Response) -> Analysis {
        analyze(response, false)
    }

    const GOOD: &str = "proxies:\n  - {name: NL, type: vless, server: nl.example.com, port: 443}\n";
    const STUB: &str =
        "proxies:\n  - {name: Limit of devices reached, type: vless, server: 0.0.0.0, port: 1}\n";

    #[test]
    fn good_config_is_usable() {
        let a = analyze_(&response(200, &[("profile-title", "VPN")], GOOD));
        assert!(a.problem.is_none());
        assert!(a.usable().is_some());
        assert_eq!(a.info.title.as_deref(), Some("VPN"));
    }

    #[test]
    fn remnawave_hwid_refusal() {
        // Remnawave answers 200 with an empty body and x-hwid-* headers.
        let a = analyze_(&response(
            200,
            &[
                ("x-hwid-active", "true"),
                ("x-hwid-not-supported", "true"),
                ("x-hwid-limit", "true"),
            ],
            "",
        ));
        assert!(matches!(a.problem, Some(Problem::Refused(_))));
        assert!(a.usable().is_none());

        let a = analyze_(&response(
            200,
            &[("x-hwid-max-devices-reached", "true")],
            STUB,
        ));
        assert!(a.problem.unwrap().message().contains("device limit"));
    }

    #[test]
    fn stub_is_refused() {
        let a = analyze_(&response(200, &[], STUB));
        assert!(matches!(a.problem, Some(Problem::Stub(_))));
        assert!(a.usable().is_none());
    }

    #[test]
    fn http_errors_and_garbage() {
        let a = analyze_(&response(403, &[], "Forbidden"));
        assert!(a.problem.unwrap().message().contains("blocks this client"));
        let a = analyze_(&response(200, &[("content-type", "text/html")], "<html>"));
        assert!(matches!(a.problem, Some(Problem::Invalid(_))));
        // Only Koala trusts Content-Type; the others parse the body.
        let html_typed = response(200, &[("content-type", "text/html")], GOOD);
        assert!(analyze(&html_typed, false).problem.is_none());
        assert!(matches!(
            analyze(&html_typed, true).problem,
            Some(Problem::Invalid(_))
        ));
    }

    #[test]
    fn redacts_tokens() {
        let url = Url::parse("https://sub.example.com/api/sub/AbCdEfGh1234?x=1").unwrap();
        assert_eq!(redact(&url), "https://sub.example.com/…1234");
    }
}
