//! Absolute `http(s)://` URLs: parsing, redirect resolution and IDN/punycode.

use anyhow::{Result, anyhow, bail};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Https,
}

impl Scheme {
    fn default_port(self) -> u16 {
        match self {
            Scheme::Http => 80,
            Scheme::Https => 443,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Https => "https",
        }
    }
}

/// An absolute `http(s)://` URL, normalised the way browsers, dart:io and Node do it
/// (lower-case host, default path `/`, fragment dropped, non-ASCII path bytes
/// percent-encoded).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Url {
    pub scheme: Scheme,
    pub host: String,
    pub port: u16,
    /// Path plus query, always starting with `/`.
    pub target: String,
}

impl Url {
    /// Parses an absolute URL. Errors never echo the input: subscription URLs carry
    /// the access token and error messages end up in logs.
    pub fn parse(input: &str) -> Result<Self> {
        let input = input.trim();
        let (scheme, rest) = if let Some(rest) = strip_prefix_ci(input, "https://") {
            (Scheme::Https, rest)
        } else if let Some(rest) = strip_prefix_ci(input, "http://") {
            (Scheme::Http, rest)
        } else {
            bail!("unsupported URL: expected http:// or https://");
        };
        let rest = rest.split('#').next().unwrap_or_default();
        let (authority, target) = match rest.find(['/', '?']) {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        if authority.contains('@') {
            bail!("credentials inside the URL are not supported");
        }
        let (host, port) = split_host_port(authority)?;
        let host = normalize_host(host)?;
        let target = if target.starts_with('?') {
            format!("/{target}")
        } else {
            target.to_owned()
        };
        Ok(Self {
            scheme,
            host,
            port: port.unwrap_or(scheme.default_port()),
            target: encode_target(&target),
        })
    }

    /// Value of the `Host` header: port only when it is not the scheme default.
    pub fn host_header(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == self.scheme.default_port() {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }

    /// Resolves a `Location` header against this URL (RFC 3986 §5.2).
    pub fn join(&self, location: &str) -> Result<Self> {
        let location = location.trim();
        let location = location.split('#').next().unwrap_or_default();
        if has_scheme(location) {
            return Self::parse(location);
        }
        if let Some(rest) = location.strip_prefix("//") {
            return Self::parse(&format!("{}://{rest}", self.scheme.as_str()));
        }
        let base_path = self.target.split('?').next().unwrap_or("/");
        let target = if location.is_empty() {
            self.target.clone()
        } else if location.starts_with('?') {
            format!("{base_path}{location}")
        } else {
            let (path, query) = match location.split_once('?') {
                Some((path, query)) => (path, Some(query)),
                None => (location, None),
            };
            let merged = if path.starts_with('/') {
                path.to_owned()
            } else {
                let dir = &base_path[..=base_path.rfind('/').unwrap_or(0)];
                format!("{dir}{path}")
            };
            let mut target = remove_dot_segments(&merged);
            if let Some(query) = query {
                target.push('?');
                target.push_str(query);
            }
            target
        };
        Ok(Self {
            target: encode_target(&target),
            ..self.clone()
        })
    }

    /// Same URL with another host (FlClashX `flclashx-newdomain` behaviour).
    pub fn with_host(&self, host: &str) -> Result<Self> {
        let host = host.trim();
        if host.contains(['/', '?', '#', '@']) {
            bail!("new domain must be a bare host[:port]");
        }
        let (host, port) = split_host_port(host)?;
        Ok(Self {
            host: normalize_host(host)?,
            port: port.unwrap_or(self.port),
            ..self.clone()
        })
    }
}

impl std::fmt::Display for Url {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}://{}{}",
            self.scheme.as_str(),
            self.host_header(),
            self.target
        )
    }
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

/// RFC 3986 `scheme ":"` prefix: a letter, then letters, digits, `+`, `-`, `.`.
fn has_scheme(reference: &str) -> bool {
    let Some(colon) = reference.find(':') else {
        return false;
    };
    let scheme = &reference[..colon];
    scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+-.".contains(&b))
}

/// RFC 3986 §5.2.4 for an absolute path.
fn remove_dot_segments(path: &str) -> String {
    let segments: Vec<&str> = path.split('/').skip(1).collect();
    let last = segments.len().saturating_sub(1);
    let mut out: Vec<&str> = Vec::with_capacity(segments.len());
    for (i, segment) in segments.iter().enumerate() {
        match *segment {
            "." | ".." => {
                if *segment == ".." {
                    out.pop();
                }
                if i == last {
                    out.push("");
                }
            }
            s => out.push(s),
        }
    }
    format!("/{}", out.join("/"))
}

fn split_host_port(authority: &str) -> Result<(&str, Option<u16>)> {
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let end = rest.find(']').ok_or_else(|| anyhow!("bad IPv6 host"))?;
        let port = match &rest[end + 1..] {
            "" => None,
            tail => Some(
                tail.strip_prefix(':')
                    .ok_or_else(|| anyhow!("bad IPv6 host"))?,
            ),
        };
        (&rest[..end], port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    let port = match port {
        Some(p) if !p.is_empty() => Some(p.parse().map_err(|_| anyhow!("bad port in URL"))?),
        _ => None,
    };
    Ok((host, port))
}

/// Lower-cases the host and converts internationalised names (`пример.рф`) to
/// their ASCII form (`xn--e1afmkfd.xn--p1ai`), as browsers, Node (Koala) and Qt
/// (Happ) do before DNS, SNI and the `Host` header. Hosts go verbatim into the
/// request, so only DNS names and IP literals are accepted afterwards (this also
/// rules out header injection).
fn normalize_host(host: &str) -> Result<String> {
    if host.is_empty() {
        bail!("URL has no host");
    }
    let host = if host.is_ascii() {
        host.to_ascii_lowercase()
    } else {
        to_ascii_domain(host).ok_or_else(|| anyhow!("URL host is not a valid domain name"))?
    };
    if !host
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-._:".contains(&b))
    {
        bail!("URL host contains invalid characters");
    }
    Ok(host)
}

/// IDNA ToASCII for the common case: lower-case, split on (ideographic) dots,
/// Punycode every non-ASCII label. The full UTS #46 mapping table (compatibility
/// characters, `ß`, NFC) is deliberately not bundled; Cyrillic, Latin with
/// diacritics and similar names come out exactly as in browsers.
fn to_ascii_domain(host: &str) -> Option<String> {
    let host = host.to_lowercase();
    let labels: Vec<String> = host
        .split(['.', '\u{3002}', '\u{ff0e}', '\u{ff61}'])
        .map(|label| {
            if label.is_ascii() {
                Some(label.to_owned())
            } else {
                Some(format!("xn--{}", punycode(label)?))
            }
        })
        .collect::<Option<_>>()?;
    labels
        .iter()
        .all(|l| !l.is_empty() && l.len() <= 63)
        .then(|| labels.join("."))
}

/// Punycode encoder (RFC 3492 §6.3).
fn punycode(input: &str) -> Option<String> {
    const BASE: u32 = 36;
    const T_MIN: u32 = 1;
    const T_MAX: u32 = 26;
    fn digit(d: u32) -> char {
        char::from(if d < 26 {
            b'a' + d as u8
        } else {
            b'0' + (d - 26) as u8
        })
    }
    fn adapt(delta: u32, points: u32, first: bool) -> u32 {
        let mut delta = if first { delta / 700 } else { delta / 2 };
        delta += delta / points;
        let mut k = 0;
        while delta > ((BASE - T_MIN) * T_MAX) / 2 {
            delta /= BASE - T_MIN;
            k += BASE;
        }
        k + (BASE - T_MIN + 1) * delta / (delta + 38)
    }
    let code_points: Vec<u32> = input.chars().map(u32::from).collect();
    let mut out: String = input.chars().filter(char::is_ascii).collect();
    let basic = out.len() as u32;
    if basic > 0 {
        out.push('-');
    }
    let (mut n, mut delta, mut bias, mut handled) = (128u32, 0u32, 72u32, basic);
    while (handled as usize) < code_points.len() {
        let m = code_points.iter().copied().filter(|&c| c >= n).min()?;
        delta = delta.checked_add((m - n).checked_mul(handled + 1)?)?;
        n = m;
        for &c in &code_points {
            if c < n {
                delta = delta.checked_add(1)?;
            }
            if c == n {
                let mut q = delta;
                let mut k = BASE;
                loop {
                    let t = if k <= bias {
                        T_MIN
                    } else if k >= bias + T_MAX {
                        T_MAX
                    } else {
                        k - bias
                    };
                    if q < t {
                        break;
                    }
                    out.push(digit(t + (q - t) % (BASE - t)));
                    q = (q - t) / (BASE - t);
                    k += BASE;
                }
                out.push(digit(q));
                bias = adapt(delta, handled + 1, handled == basic);
                delta = 0;
                handled += 1;
            }
        }
        delta = delta.checked_add(1)?;
        n += 1;
    }
    Some(out)
}

fn encode_target(target: &str) -> String {
    let mut out = String::with_capacity(target.len());
    for &b in target.as_bytes() {
        if (0x21..0x7f).contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        let url = Url::parse("HTTPS://Sub.Example.com/abc?x=1#frag").unwrap();
        assert_eq!(url.scheme, Scheme::Https);
        assert_eq!(url.host, "sub.example.com");
        assert_eq!(url.port, 443);
        assert_eq!(url.target, "/abc?x=1");
        assert_eq!(url.host_header(), "sub.example.com");
        assert_eq!(url.to_string(), "https://sub.example.com/abc?x=1");

        let url = Url::parse("http://127.0.0.1:8080").unwrap();
        assert_eq!(url.target, "/");
        assert_eq!(url.host_header(), "127.0.0.1:8080");

        let url = Url::parse("http://[::1]:9090/version").unwrap();
        assert_eq!(url.host, "::1");
        assert_eq!(url.host_header(), "[::1]:9090");

        assert_eq!(Url::parse("https://h/?q").unwrap().target, "/?q");
        assert_eq!(
            Url::parse("https://h/путь").unwrap().target,
            "/%D0%BF%D1%83%D1%82%D1%8C"
        );
        assert!(Url::parse("ftp://h/").is_err());
        assert!(Url::parse("https://u:p@h/").is_err());
        assert!(Url::parse("https://пример..рф/").is_err());
        assert!(Url::parse("https://a b/").is_err());
        assert!(Url::parse("https://a\r\nX-Evil: 1/").is_err());
        assert!(Url::parse("https://[::1]x/").is_err());
    }

    #[test]
    fn internationalised_domains_become_punycode() {
        // Reference values: RFC 3492 samples and what browsers send.
        assert_eq!(punycode("пример").as_deref(), Some("e1afmkfd"));
        assert_eq!(punycode("bücher").as_deref(), Some("bcher-kva"));
        assert_eq!(punycode("münchen").as_deref(), Some("mnchen-3ya"));
        assert_eq!(punycode("президент").as_deref(), Some("d1abbgf6aiiy"));
        let url = Url::parse("https://ПРИМЕР.рф:8443/sub/токен?x=1").unwrap();
        assert_eq!(url.host, "xn--e1afmkfd.xn--p1ai");
        assert_eq!(url.host_header(), "xn--e1afmkfd.xn--p1ai:8443");
        assert_eq!(url.target, "/sub/%D1%82%D0%BE%D0%BA%D0%B5%D0%BD?x=1");
        assert_eq!(
            Url::parse("https://sub.пример。рф/").unwrap().host,
            "sub.xn--e1afmkfd.xn--p1ai"
        );
        let base = Url::parse("https://a.com/x").unwrap();
        assert_eq!(
            base.with_host("пример.рф").unwrap().host,
            "xn--e1afmkfd.xn--p1ai"
        );
    }

    #[test]
    fn url_errors_do_not_leak_the_input() {
        for bad in [
            "ftp://h/SECRET",
            "https://h:SECRET/",
            "https://SECRET\u{1}/",
        ] {
            let err = format!("{:#}", Url::parse(bad).unwrap_err());
            assert!(!err.contains("SECRET"), "{err}");
        }
    }

    #[test]
    fn joins_redirects() {
        let base = Url::parse("https://a.com/sub/abc?x").unwrap();
        let join = |loc: &str| base.join(loc).unwrap().to_string();
        assert_eq!(join("/new"), "https://a.com/new");
        assert_eq!(join("def"), "https://a.com/sub/def");
        assert_eq!(join("//b.com/z"), "https://b.com/z");
        assert_eq!(join("http://c.com:81/"), "http://c.com:81/");
        assert_eq!(join("HTTPS://D.com/q"), "https://d.com/q");
        assert_eq!(join("?y=1"), "https://a.com/sub/abc?y=1");
        assert_eq!(join(""), "https://a.com/sub/abc?x");
        assert_eq!(join("../up"), "https://a.com/up");
        assert_eq!(join("./same/../x?q=1#f"), "https://a.com/sub/x?q=1");
        assert_eq!(join("/../../etc"), "https://a.com/etc");
        // A relative reference whose query holds a URL is not an absolute URL.
        assert_eq!(
            join("/go?to=https://evil.com/"),
            "https://a.com/go?to=https://evil.com/"
        );
        assert!(base.join("javascript:alert(1)").is_err());
    }

    #[test]
    fn swaps_hosts() {
        let base = Url::parse("https://a.com/sub/abc?x").unwrap();
        assert_eq!(
            base.with_host("new.com").unwrap().to_string(),
            "https://new.com/sub/abc?x"
        );
        assert_eq!(
            base.with_host("new.com:8443").unwrap().to_string(),
            "https://new.com:8443/sub/abc?x"
        );
        for bad in ["evil.com/x", "u@evil.com", "a.com?x", "", "a b"] {
            assert!(base.with_host(bad).is_err(), "{bad}");
        }
    }
}
