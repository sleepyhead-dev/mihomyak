//! Minimal blocking HTTP/1.1 client.
//!
//! Why not `ureq`/`reqwest`: faithful client emulation needs byte-level control over
//! the request head (header order and letter case differ between FlClashX's dart:io
//! and Koala's axios), and every mainstream client normalises headers through the
//! `http` crate (lower-case names, own ordering, implicit headers). The protocol
//! subset needed here — one request per connection, `Content-Length`/chunked/close
//! bodies, gzip/deflate/br/zstd — is small enough to own.
//!
//! The caller supplies the complete, ordered header list (including `Host`); the
//! client adds nothing except `Content-Length` for requests with a body.
//!
//! The server is untrusted: every line, header block, chunk, trailer section and the
//! decoded body are bounded, and the whole exchange has a wall-clock deadline so a
//! server trickling one byte per `io_timeout` cannot hold the supervisor forever.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

const MAX_HEAD_BYTES: usize = 64 * 1024;
const MAX_LINE_BYTES: usize = 16 * 1024;
const MAX_TRAILERS: usize = 64;
const MAX_INTERIM_RESPONSES: usize = 8;

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

/// Where to open the connection.
#[derive(Clone, Debug)]
pub enum Endpoint {
    Tcp { host: String, port: u16, tls: bool },
    Unix(PathBuf),
}

impl From<&Url> for Endpoint {
    fn from(url: &Url) -> Self {
        Endpoint::Tcp {
            host: url.host.clone(),
            port: url.port,
            tls: url.scheme == Scheme::Https,
        }
    }
}

pub struct Request<'a> {
    pub method: &'a str,
    /// Request target, e.g. `/sub/abc?x=1`.
    pub target: &'a str,
    /// Complete ordered header list, written verbatim.
    pub headers: &'a [(String, String)],
    pub body: &'a [u8],
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub reason: String,
    /// Headers in wire order with original letter case.
    pub headers: Vec<(String, String)>,
    /// Body with `Content-Encoding` already removed.
    pub body: Vec<u8>,
    /// Address of the server that answered (direct TCP only; `None` through a proxy
    /// or a unix socket).
    pub peer: Option<IpAddr>,
}

impl Response {
    /// First header with this name (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Clone, Debug)]
pub struct Client {
    pub connect_timeout: Duration,
    /// Longest silence between two reads or writes.
    pub io_timeout: Duration,
    /// Wall-clock limit for the whole exchange, connect included.
    pub total_timeout: Duration,
    /// Optional `http://host:port` proxy used via `CONNECT` for TCP endpoints.
    pub proxy: Option<Url>,
    pub max_body: usize,
    /// `SO_MARK` for outgoing TCP connections, so the gateway kill switch lets
    /// them through (needs `CAP_NET_ADMIN`; without it the mark is skipped).
    pub mark: Option<u32>,
}

impl Default for Client {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(15),
            io_timeout: Duration::from_secs(30),
            total_timeout: Duration::from_secs(90),
            proxy: None,
            max_body: 32 * 1024 * 1024,
            mark: None,
        }
    }
}

impl Client {
    pub fn send(&self, endpoint: &Endpoint, req: &Request<'_>) -> Result<Response> {
        let deadline = Instant::now() + self.total_timeout;
        let (inner, peer) = self.connect(endpoint, deadline)?;
        let mut stream = Stream {
            inner,
            deadline,
            io_timeout: self.io_timeout,
        };
        let mut head = format!("{} {} HTTP/1.1\r\n", req.method, req.target);
        for (name, value) in req.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        let has_length = req
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("content-length"));
        if !req.body.is_empty() && !has_length {
            head.push_str(&format!("Content-Length: {}\r\n", req.body.len()));
        }
        head.push_str("\r\n");
        let mut wire = head.into_bytes();
        wire.extend_from_slice(req.body);
        stream.write_all(&wire).context("send request")?;
        stream.flush()?;
        let mut response = read_response(&mut stream, req.method, self.max_body)?;
        response.peer = peer;
        Ok(response)
    }

    fn connect(&self, endpoint: &Endpoint, deadline: Instant) -> Result<(Inner, Option<IpAddr>)> {
        match endpoint {
            Endpoint::Unix(path) => {
                let sock = UnixStream::connect(path)
                    .with_context(|| format!("connect {}", path.display()))?;
                Ok((Inner::Unix(sock), None))
            }
            Endpoint::Tcp { host, port, tls } => {
                let (tcp, peer) = match &self.proxy {
                    Some(proxy) => (self.tunnel(proxy, host, *port, deadline)?, None),
                    None => {
                        let tcp = self.dial(host, *port, deadline)?;
                        let peer = tcp.peer_addr().ok().map(|a| a.ip());
                        (tcp, peer)
                    }
                };
                if !tls {
                    return Ok((Inner::Tcp(tcp), peer));
                }
                let name = rustls::pki_types::ServerName::try_from(host.clone())
                    .context("invalid TLS server name")?;
                let conn = rustls::ClientConnection::new(tls_config()?, name)?;
                Ok((
                    Inner::Tls(Box::new(rustls::StreamOwned::new(conn, tcp))),
                    peer,
                ))
            }
        }
    }

    fn dial(&self, host: &str, port: u16, deadline: Instant) -> Result<TcpStream> {
        let addrs = (host, port)
            .to_socket_addrs()
            .with_context(|| format!("resolve {host}"))?;
        let mut last_err = None;
        for addr in addrs {
            let timeout = self.connect_timeout.min(remaining(deadline)?);
            let connected = match self.mark {
                Some(mark) => connect_marked(&addr, timeout, mark),
                None => TcpStream::connect_timeout(&addr, timeout),
            };
            match connected {
                Ok(tcp) => {
                    tcp.set_nodelay(true)?;
                    return Ok(tcp);
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(match last_err {
            Some(e) => anyhow!(e).context(format!("connect {host}:{port}")),
            None => anyhow!("{host} resolved to no addresses"),
        })
    }

    fn tunnel(&self, proxy: &Url, host: &str, port: u16, deadline: Instant) -> Result<TcpStream> {
        let tcp = self.dial(&proxy.host, proxy.port, deadline)?;
        let mut stream = Stream {
            inner: Inner::Tcp(tcp),
            deadline,
            io_timeout: self.io_timeout,
        };
        let authority = if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        write!(
            stream,
            "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n"
        )?;
        // Read byte-wise: nothing may be consumed past the proxy's response head.
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if head.len() > MAX_HEAD_BYTES || stream.read(&mut byte)? == 0 {
                bail!("proxy closed the connection during CONNECT");
            }
            head.push(byte[0]);
        }
        let status_line = String::from_utf8_lossy(&head);
        let status = status_line.split_whitespace().nth(1).unwrap_or_default();
        if status != "200" {
            bail!(
                "proxy refused CONNECT: {}",
                crate::util::sanitize(status_line.lines().next().unwrap_or_default())
            );
        }
        let Inner::Tcp(tcp) = stream.inner else {
            unreachable!()
        };
        Ok(tcp)
    }
}

/// `TcpStream::connect_timeout` with `SO_MARK` set before the SYN leaves: std
/// cannot configure a socket before connecting, so this is done with libc.
fn connect_marked(addr: &SocketAddr, timeout: Duration, mark: u32) -> io::Result<TcpStream> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    fn check(ret: libc::c_int) -> io::Result<libc::c_int> {
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(ret)
        }
    }
    // SAFETY: plain socket syscalls on a descriptor owned by `fd`; the sockaddr
    // structs are fully initialised (zeroed, then every relevant field set) and
    // passed with their exact sizes.
    unsafe {
        let family = if addr.is_ipv4() {
            libc::AF_INET
        } else {
            libc::AF_INET6
        };
        let raw = check(libc::socket(
            family,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        ))?;
        let fd = OwnedFd::from_raw_fd(raw);
        let set_mark = libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_MARK,
            (&raw const mark).cast(),
            std::mem::size_of::<u32>() as libc::socklen_t,
        );
        if set_mark != 0 {
            let err = io::Error::last_os_error();
            // EPERM: no CAP_NET_ADMIN; ENOPROTOOPT: e.g. qemu-user emulation.
            if !matches!(err.raw_os_error(), Some(libc::EPERM | libc::ENOPROTOOPT)) {
                return Err(err);
            }
            crate::debug!("cannot set the kill-switch mark ({err}); connecting without it");
        }
        let mut storage: libc::sockaddr_storage = std::mem::zeroed();
        let len = match addr {
            SocketAddr::V4(a) => {
                let sin = &mut *(&raw mut storage).cast::<libc::sockaddr_in>();
                sin.sin_family = libc::AF_INET as libc::sa_family_t;
                sin.sin_port = a.port().to_be();
                sin.sin_addr.s_addr = u32::from_ne_bytes(a.ip().octets());
                std::mem::size_of::<libc::sockaddr_in>()
            }
            SocketAddr::V6(a) => {
                let sin6 = &mut *(&raw mut storage).cast::<libc::sockaddr_in6>();
                sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
                sin6.sin6_port = a.port().to_be();
                sin6.sin6_flowinfo = a.flowinfo();
                sin6.sin6_addr.s6_addr = a.ip().octets();
                sin6.sin6_scope_id = a.scope_id();
                std::mem::size_of::<libc::sockaddr_in6>()
            }
        };
        let ret = libc::connect(
            fd.as_raw_fd(),
            (&raw const storage).cast(),
            len as libc::socklen_t,
        );
        if ret != 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EINPROGRESS) {
                return Err(err);
            }
            let mut pfd = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            let millis = timeout.as_millis().clamp(1, libc::c_int::MAX as u128) as libc::c_int;
            if check(libc::poll(&mut pfd, 1, millis))? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "connection timed out",
                ));
            }
            let mut so_error: libc::c_int = 0;
            let mut optlen = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            check(libc::getsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&raw mut so_error).cast(),
                &mut optlen,
            ))?;
            if so_error != 0 {
                return Err(io::Error::from_raw_os_error(so_error));
            }
        }
        let stream = TcpStream::from(fd);
        stream.set_nonblocking(false)?;
        Ok(stream)
    }
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "HTTP request deadline exceeded",
        ));
    }
    Ok(left)
}

enum Inner {
    Tcp(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
    Unix(UnixStream),
}

/// A connection whose socket timeouts are re-armed before every read and write, so
/// each operation waits at most `min(io_timeout, time left until deadline)`.
struct Stream {
    inner: Inner,
    deadline: Instant,
    io_timeout: Duration,
}

impl Stream {
    fn arm(&self) -> io::Result<()> {
        let timeout = Some(self.io_timeout.min(remaining(self.deadline)?));
        match &self.inner {
            Inner::Tcp(s) => {
                s.set_read_timeout(timeout)?;
                s.set_write_timeout(timeout)
            }
            Inner::Tls(s) => {
                s.sock.set_read_timeout(timeout)?;
                s.sock.set_write_timeout(timeout)
            }
            Inner::Unix(s) => {
                s.set_read_timeout(timeout)?;
                s.set_write_timeout(timeout)
            }
        }
    }
}

impl Stream {
    /// Socket timeouts surface as `EAGAIN`; name them. The socket waits for the
    /// shorter of the idle timeout and the request deadline: say which one ran out.
    fn timed_out(&self, e: io::Error) -> io::Error {
        if !matches!(
            e.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ) {
            return e;
        }
        // Socket timeouts are truncated to microseconds: allow for an early wake-up.
        let slack = Duration::from_millis(1);
        let deadline = self.deadline.checked_sub(slack).unwrap_or(self.deadline);
        if let Err(exceeded) = remaining(deadline) {
            return exceeded;
        }
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("no data from the server for {:?}", self.io_timeout),
        )
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.arm()?;
        let result = match &mut self.inner {
            Inner::Tcp(s) => s.read(buf),
            Inner::Tls(s) => s.read(buf),
            Inner::Unix(s) => s.read(buf),
        };
        result.map_err(|e| self.timed_out(e))
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.arm()?;
        let result = match &mut self.inner {
            Inner::Tcp(s) => s.write(buf),
            Inner::Tls(s) => s.write(buf),
            Inner::Unix(s) => s.write(buf),
        };
        result.map_err(|e| self.timed_out(e))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.arm()?;
        let result = match &mut self.inner {
            Inner::Tcp(s) => s.flush(),
            Inner::Tls(s) => s.flush(),
            Inner::Unix(s) => s.flush(),
        };
        result.map_err(|e| self.timed_out(e))
    }
}

fn tls_config() -> Result<Arc<rustls::ClientConfig>> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    if let Some(config) = CONFIG.get() {
        return Ok(config.clone());
    }
    let mut roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    // Extra trust anchors for TLS-intercepting networks (same variable as OpenSSL).
    if let Some(path) = std::env::var_os("SSL_CERT_FILE") {
        use rustls::pki_types::{CertificateDer, pem::PemObject};
        for cert in CertificateDer::pem_file_iter(&path)
            .with_context(|| format!("read SSL_CERT_FILE {}", Path::new(&path).display()))?
        {
            roots.add(cert?)?;
        }
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(CONFIG.get_or_init(|| Arc::new(config)).clone())
}

/// Reads one line (terminator included) of at most `limit` bytes; empty at EOF.
fn read_line(reader: &mut impl BufRead, limit: usize) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    reader
        .by_ref()
        .take(limit as u64 + 1)
        .read_until(b'\n', &mut line)?;
    if line.len() > limit {
        bail!("response line exceeds {limit} bytes");
    }
    Ok(line)
}

fn trim_line(line: &[u8]) -> String {
    String::from_utf8_lossy(line)
        .trim_end_matches(['\r', '\n'])
        .to_owned()
}

struct Head {
    status: u16,
    reason: String,
    headers: Vec<(String, String)>,
}

fn read_head(reader: &mut impl BufRead) -> Result<Head> {
    let mut head_bytes = 0usize;
    let mut next_line = |reader: &mut _| -> Result<String> {
        let line = read_line(reader, MAX_LINE_BYTES)?;
        if line.is_empty() {
            bail!("connection closed before the response head was complete");
        }
        head_bytes += line.len();
        if head_bytes > MAX_HEAD_BYTES {
            bail!("response head exceeds {MAX_HEAD_BYTES} bytes");
        }
        Ok(trim_line(&line))
    };

    let status_line = next_line(reader)?;
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    let status = parts.next().and_then(|s| s.parse::<u16>().ok());
    let (true, Some(status @ 100..=999)) = (version.starts_with("HTTP/1."), status) else {
        bail!("not an HTTP/1.x response");
    };
    let reason = crate::util::sanitize(parts.next().unwrap_or_default());

    let mut headers = Vec::new();
    loop {
        let line = next_line(reader)?;
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
    }
    Ok(Head {
        status,
        reason,
        headers,
    })
}

fn read_response(stream: &mut impl Read, method: &str, max_body: usize) -> Result<Response> {
    let mut reader = BufReader::new(stream);
    let mut interim = 0;
    let head = loop {
        let head = read_head(&mut reader)?;
        // 1xx other than 101 precede the real response (RFC 9110 §15.2).
        if (100..200).contains(&head.status) && head.status != 101 {
            interim += 1;
            if interim > MAX_INTERIM_RESPONSES {
                bail!("too many interim (1xx) responses");
            }
            continue;
        }
        break head;
    };
    let header = |name: &str| {
        head.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };

    let no_body = method.eq_ignore_ascii_case("HEAD")
        || head.status == 101
        || head.status == 204
        || head.status == 304;
    let body = if no_body {
        Vec::new()
    } else if header("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
    {
        read_chunked(&mut reader, max_body)?
    } else if let Some(len) = header("content-length") {
        let len: usize = len.parse().map_err(|_| anyhow!("bad Content-Length"))?;
        if len > max_body {
            bail!("response body of {len} bytes exceeds the {max_body} byte limit");
        }
        let mut body = vec![0; len];
        reader.read_exact(&mut body).context("read response body")?;
        body
    } else {
        read_to_close(&mut reader, max_body)?
    };

    let body = match header("content-encoding") {
        Some(encoding) if !body.is_empty() => decode(body, encoding, max_body)?,
        _ => body,
    };
    Ok(Response {
        status: head.status,
        reason: head.reason,
        headers: head.headers,
        body,
        peer: None,
    })
}

fn read_chunked(reader: &mut impl BufRead, max_body: usize) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let line = read_line(reader, MAX_LINE_BYTES)?;
        if line.is_empty() {
            bail!("connection closed inside a chunked body");
        }
        let line = trim_line(&line);
        let size = line.split(';').next().unwrap_or_default().trim();
        if size.is_empty() || size.len() > 16 || !size.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("bad chunk size");
        }
        let size = u64::from_str_radix(size, 16)?;
        if size == 0 {
            // Trailer section ends with an empty line (or EOF from sloppy servers).
            for _ in 0..=MAX_TRAILERS {
                let line = read_line(reader, MAX_LINE_BYTES)?;
                if line.is_empty() || trim_line(&line).is_empty() {
                    return Ok(body);
                }
            }
            bail!("more than {MAX_TRAILERS} trailer fields");
        }
        let end = usize::try_from(size)
            .ok()
            .and_then(|size| body.len().checked_add(size))
            .filter(|&end| end <= max_body)
            .ok_or_else(|| anyhow!("response body exceeds the {max_body} byte limit"))?;
        let start = body.len();
        body.resize(end, 0);
        reader
            .read_exact(&mut body[start..])
            .context("read chunk")?;
        read_line(reader, MAX_LINE_BYTES)?;
    }
}

fn read_to_close(reader: &mut impl Read, max_body: usize) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return Ok(body),
            Ok(n) => {
                if body.len() + n > max_body {
                    bail!("response body exceeds the {max_body} byte limit");
                }
                body.extend_from_slice(&buf[..n]);
            }
            // Many servers close TLS without close_notify; the body is complete anyway.
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(body),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e).context("read response body"),
        }
    }
}

/// Undoes `Content-Encoding` (listed in application order, so undo in reverse).
fn decode(mut body: Vec<u8>, encodings: &str, max_body: usize) -> Result<Vec<u8>> {
    for encoding in encodings.rsplit(',').map(|e| e.trim().to_ascii_lowercase()) {
        body = match encoding.as_str() {
            "" | "identity" => body,
            "gzip" | "x-gzip" => inflate(flate2::read::MultiGzDecoder::new(&body[..]), max_body)?,
            "deflate" => {
                // RFC says zlib-wrapped, some servers send raw deflate.
                inflate(flate2::read::ZlibDecoder::new(&body[..]), max_body)
                    .or_else(|_| inflate(flate2::read::DeflateDecoder::new(&body[..]), max_body))?
            }
            "br" => inflate(
                brotli_decompressor::Decompressor::new(&body[..], 4096),
                max_body,
            )?,
            "zstd" => inflate(
                ruzstd::decoding::StreamingDecoder::new(&body[..])
                    .map_err(|e| anyhow!("zstd: {e}"))?,
                max_body,
            )?,
            _ => bail!("unsupported Content-Encoding"),
        };
    }
    Ok(body)
}

/// Reads a decompressor to the end, refusing output larger than `max` bytes.
pub(crate) fn inflate(decoder: impl Read, max: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    decoder
        .take(max as u64 + 1)
        .read_to_end(&mut out)
        .context("decompress")?;
    if out.len() > max {
        bail!("decompressed data exceeds the {max} byte limit");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

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

    fn parse(raw: &[u8]) -> Response {
        read_response(&mut Cursor::new(raw.to_vec()), "GET", 1 << 20).unwrap()
    }

    fn parse_err(raw: &[u8], max: usize) -> String {
        format!(
            "{:#}",
            read_response(&mut Cursor::new(raw.to_vec()), "GET", max).unwrap_err()
        )
    }

    #[test]
    fn reads_content_length_body() {
        let r = parse(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-A: b\r\n\r\nhelloEXTRA");
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"hello");
        assert_eq!(r.header("x-a"), Some("b"));
    }

    #[test]
    fn reads_chunked_body() {
        let r = parse(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5;ext\r\nhello\r\n6\r\n world\r\n0\r\nX-T: 1\r\n\r\n");
        assert_eq!(r.body, b"hello world");
    }

    #[test]
    fn rejects_hostile_chunks() {
        let chunked =
            |rest: &str| format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{rest}");
        // Would overflow `len + size` without checked arithmetic.
        let e = parse_err(chunked("1\r\na\r\nffffffffffffffff\r\n").as_bytes(), 100);
        assert!(e.contains("limit"), "{e}");
        assert!(parse_err(chunked("10000000000000000\r\n").as_bytes(), 100).contains("chunk size"));
        assert!(parse_err(chunked("-1\r\n").as_bytes(), 100).contains("chunk size"));
        assert!(parse_err(chunked("5\r\nhel").as_bytes(), 100).contains("chunk"));
        let trailers = "X: 1\r\n".repeat(MAX_TRAILERS + 1);
        assert!(
            parse_err(chunked(&format!("0\r\n{trailers}\r\n")).as_bytes(), 100).contains("trailer")
        );
    }

    #[test]
    fn bounds_lines() {
        let long = format!(
            "HTTP/1.1 200 OK\r\nX: {}\r\n\r\n",
            "a".repeat(MAX_LINE_BYTES)
        );
        assert!(parse_err(long.as_bytes(), 100).contains("line"));
        let many = format!(
            "HTTP/1.1 200 OK\r\n{}\r\n",
            "X: aaaaaaaaaaaaaaaa\r\n".repeat(4000)
        );
        assert!(parse_err(many.as_bytes(), 100).contains("head"));
    }

    #[test]
    fn skips_interim_responses() {
        let r = parse(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 103 Early Hints\r\nLink: x\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
        assert_eq!((r.status, &r.body[..]), (200, &b"ok"[..]));
        let flood = "HTTP/1.1 100 Continue\r\n\r\n".repeat(MAX_INTERIM_RESPONSES + 1);
        assert!(parse_err(flood.as_bytes(), 100).contains("interim"));
    }

    #[test]
    fn reads_until_close_and_head() {
        let r = parse(b"HTTP/1.0 404 Not Found\r\n\r\nnope");
        assert_eq!(
            (r.status, r.reason.as_str(), &r.body[..]),
            (404, "Not Found", &b"nope"[..])
        );
        let r = read_response(
            &mut Cursor::new(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n".to_vec()),
            "HEAD",
            100,
        )
        .unwrap();
        assert!(r.body.is_empty());
        let r = parse(b"HTTP/1.1 204 No Content\r\nContent-Encoding: gzip\r\n\r\n");
        assert!(r.body.is_empty());
    }

    #[test]
    fn empty_encoded_body_is_not_decoded() {
        let r = parse(b"HTTP/1.1 200 OK\r\nContent-Encoding: br\r\nContent-Length: 0\r\n\r\n");
        assert!(r.body.is_empty());
    }

    #[test]
    fn decodes_gzip() {
        use flate2::{Compression, write::GzEncoder};
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(b"proxies: []").unwrap();
        let gz = enc.finish().unwrap();
        let mut raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
            gz.len()
        )
        .into_bytes();
        raw.extend_from_slice(&gz);
        assert_eq!(parse(&raw).body, b"proxies: []");
    }

    #[test]
    fn decodes_zstd() {
        let packed = ruzstd::encoding::compress_to_vec(
            &b"proxies: []"[..],
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        assert_eq!(decode(packed, "zstd", 1024).unwrap(), b"proxies: []");
    }

    #[test]
    fn enforces_body_limit() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 50\r\n\r\n".to_vec();
        assert!(read_response(&mut Cursor::new(raw), "GET", 10).is_err());
    }

    #[test]
    fn rejects_garbage() {
        assert!(read_response(&mut Cursor::new(b"SSH-2.0\r\n\r\n".to_vec()), "GET", 10).is_err());
        assert!(
            read_response(
                &mut Cursor::new(b"HTTP/1.1 20 OK\r\n\r\n".to_vec()),
                "GET",
                10
            )
            .is_err()
        );
        assert!(read_response(&mut Cursor::new(Vec::new()), "GET", 10).is_err());
    }

    #[test]
    fn marked_connections_work() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Without CAP_NET_ADMIN the mark is skipped, with it the socket is marked;
        // either way the connection must behave like a normal one.
        let mut stream = connect_marked(&addr, Duration::from_secs(2), 0x6d796b).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        stream.write_all(b"ping").unwrap();
        let mut buf = [0u8; 4];
        server.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"ping");
        drop(listener);
        let closed = connect_marked(&addr, Duration::from_secs(2), 1).unwrap_err();
        assert_eq!(closed.kind(), io::ErrorKind::ConnectionRefused);
    }

    #[test]
    fn total_deadline_stops_a_trickling_server() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let writer = std::thread::spawn(move || {
            let _ = server.write_all(b"HTTP/1.1 200 OK\r\n");
            for _ in 0..100 {
                if server.write_all(b"X: y\r\n").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let mut stream = Stream {
            inner: Inner::Unix(client),
            deadline: Instant::now() + Duration::from_millis(200),
            io_timeout: Duration::from_secs(5),
        };
        let started = Instant::now();
        assert!(read_response(&mut stream, "GET", 1024).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(stream);
        writer.join().unwrap();
    }

    #[test]
    fn timeouts_name_what_ran_out() {
        let read = |deadline, io_timeout| {
            let (client, _server) = UnixStream::pair().unwrap();
            let mut stream = Stream {
                inner: Inner::Unix(client),
                deadline: Instant::now() + deadline,
                io_timeout,
            };
            stream.read(&mut [0; 8]).unwrap_err().to_string()
        };
        let short = Duration::from_millis(100);
        let long = Duration::from_secs(5);
        assert_eq!(read(short, long), "HTTP request deadline exceeded");
        assert_eq!(read(long, short), "no data from the server for 100ms");
    }
}
