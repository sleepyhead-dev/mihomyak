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

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

const MAX_HEAD_BYTES: usize = 64 * 1024;

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

    fn as_str(self) -> &'static str {
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
    pub fn parse(input: &str) -> Result<Self> {
        let input = input.trim();
        let (scheme, rest) = if let Some(rest) = strip_prefix_ci(input, "https://") {
            (Scheme::Https, rest)
        } else if let Some(rest) = strip_prefix_ci(input, "http://") {
            (Scheme::Http, rest)
        } else {
            bail!("unsupported URL {input:?}: expected http:// or https://");
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
        if host.is_empty() {
            bail!("URL {input:?} has no host");
        }
        if !host.is_ascii() {
            bail!("internationalised domain {host:?} is not supported, use its punycode form");
        }
        let target = if target.starts_with('?') {
            format!("/{target}")
        } else {
            target.to_owned()
        };
        Ok(Self {
            scheme,
            host: host.to_ascii_lowercase(),
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

    /// Resolves a `Location` header against this URL.
    pub fn join(&self, location: &str) -> Result<Self> {
        let location = location.trim();
        if location.contains("://") {
            return Self::parse(location);
        }
        if let Some(rest) = location.strip_prefix("//") {
            return Self::parse(&format!("{}://{rest}", self.scheme.as_str()));
        }
        let target = if location.starts_with('/') {
            location.to_owned()
        } else {
            let path = self.target.split('?').next().unwrap_or("/");
            let dir = &path[..=path.rfind('/').unwrap_or(0)];
            format!("{dir}{location}")
        };
        Ok(Self {
            target: encode_target(&target),
            ..self.clone()
        })
    }

    /// Same URL with another host (FlClashX `flclashx-newdomain` behaviour).
    pub fn with_host(&self, host: &str) -> Result<Self> {
        let (host, port) = split_host_port(host)?;
        Ok(Self {
            host: host.to_ascii_lowercase(),
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

fn split_host_port(authority: &str) -> Result<(&str, Option<u16>)> {
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let end = rest
            .find(']')
            .ok_or_else(|| anyhow!("bad IPv6 host {authority:?}"))?;
        (&rest[..end], rest[end + 1..].strip_prefix(':'))
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    let port = match port {
        Some(p) if !p.is_empty() => Some(p.parse().with_context(|| format!("bad port {p:?}"))?),
        _ => None,
    };
    Ok((host, port))
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
    pub io_timeout: Duration,
    /// Optional `http://host:port` proxy used via `CONNECT` for TCP endpoints.
    pub proxy: Option<Url>,
    pub max_body: usize,
}

impl Default for Client {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(15),
            io_timeout: Duration::from_secs(60),
            proxy: None,
            max_body: 32 * 1024 * 1024,
        }
    }
}

impl Client {
    pub fn send(&self, endpoint: &Endpoint, req: &Request<'_>) -> Result<Response> {
        let mut stream = self.connect(endpoint)?;
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
        read_response(&mut stream, req.method, self.max_body)
    }

    fn connect(&self, endpoint: &Endpoint) -> Result<Stream> {
        match endpoint {
            Endpoint::Unix(path) => {
                let sock = UnixStream::connect(path)
                    .with_context(|| format!("connect {}", path.display()))?;
                sock.set_read_timeout(Some(self.io_timeout))?;
                sock.set_write_timeout(Some(self.io_timeout))?;
                Ok(Stream::Unix(sock))
            }
            Endpoint::Tcp { host, port, tls } => {
                let tcp = match &self.proxy {
                    Some(proxy) => self.tunnel(proxy, host, *port)?,
                    None => self.dial(host, *port)?,
                };
                if !tls {
                    return Ok(Stream::Tcp(tcp));
                }
                let name = rustls::pki_types::ServerName::try_from(host.clone())
                    .with_context(|| format!("invalid TLS server name {host:?}"))?;
                let conn = rustls::ClientConnection::new(tls_config()?, name)?;
                Ok(Stream::Tls(Box::new(rustls::StreamOwned::new(conn, tcp))))
            }
        }
    }

    fn dial(&self, host: &str, port: u16) -> Result<TcpStream> {
        let addrs = (host, port)
            .to_socket_addrs()
            .with_context(|| format!("resolve {host}"))?;
        let mut last_err = None;
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, self.connect_timeout) {
                Ok(tcp) => {
                    tcp.set_read_timeout(Some(self.io_timeout))?;
                    tcp.set_write_timeout(Some(self.io_timeout))?;
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

    fn tunnel(&self, proxy: &Url, host: &str, port: u16) -> Result<TcpStream> {
        let mut tcp = self.dial(&proxy.host, proxy.port)?;
        let authority = if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        write!(
            tcp,
            "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n"
        )?;
        // Read byte-wise: nothing may be consumed past the proxy's response head.
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if head.len() > MAX_HEAD_BYTES || tcp.read(&mut byte)? == 0 {
                bail!("proxy closed the connection during CONNECT");
            }
            head.push(byte[0]);
        }
        let status_line = String::from_utf8_lossy(&head);
        let status = status_line.split_whitespace().nth(1).unwrap_or_default();
        if status != "200" {
            bail!(
                "proxy refused CONNECT: {}",
                status_line.lines().next().unwrap_or_default()
            );
        }
        Ok(tcp)
    }
}

enum Stream {
    Tcp(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
    Unix(UnixStream),
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Tcp(s) => s.read(buf),
            Stream::Tls(s) => s.read(buf),
            Stream::Unix(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Tcp(s) => s.write(buf),
            Stream::Tls(s) => s.write(buf),
            Stream::Unix(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Tcp(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
            Stream::Unix(s) => s.flush(),
        }
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

fn read_response(stream: &mut impl Read, method: &str, max_body: usize) -> Result<Response> {
    let mut reader = BufReader::new(stream);
    let mut head_bytes = 0usize;
    let mut next_line = |reader: &mut BufReader<_>| -> Result<String> {
        let mut line = Vec::new();
        let n = reader.read_until(b'\n', &mut line)?;
        head_bytes += n;
        if n == 0 {
            bail!("connection closed before the response head was complete");
        }
        if head_bytes > MAX_HEAD_BYTES {
            bail!("response head exceeds {MAX_HEAD_BYTES} bytes");
        }
        Ok(String::from_utf8_lossy(&line)
            .trim_end_matches(['\r', '\n'])
            .to_owned())
    };

    let status_line = next_line(&mut reader)?;
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/1.") {
        bail!("not an HTTP/1.x response: {status_line:?}");
    }
    let status: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow!("bad status line {status_line:?}"))?;
    let reason = parts.next().unwrap_or_default().to_owned();

    let mut headers = Vec::new();
    loop {
        let line = next_line(&mut reader)?;
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
    }
    let response_header = |name: &str| {
        headers
            .iter()
            .find(|(k, _): &&(String, String)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };

    let no_body = method.eq_ignore_ascii_case("HEAD")
        || (100..200).contains(&status)
        || status == 204
        || status == 304;
    let body = if no_body {
        Vec::new()
    } else if response_header("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
    {
        read_chunked(&mut reader, max_body)?
    } else if let Some(len) = response_header("content-length") {
        let len: usize = len
            .parse()
            .with_context(|| format!("bad Content-Length {len:?}"))?;
        if len > max_body {
            bail!("response body of {len} bytes exceeds the {max_body} byte limit");
        }
        let mut body = vec![0; len];
        reader.read_exact(&mut body).context("read response body")?;
        body
    } else {
        read_to_close(&mut reader, max_body)?
    };

    let body = match response_header("content-encoding") {
        Some(encoding) => decode(body, encoding, max_body)?,
        None => body,
    };
    Ok(Response {
        status,
        reason,
        headers,
        body,
    })
}

fn read_chunked(reader: &mut impl BufRead, max_body: usize) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let size_str = line.trim().split(';').next().unwrap_or_default();
        let size = usize::from_str_radix(size_str, 16)
            .with_context(|| format!("bad chunk size {size_str:?}"))?;
        if size == 0 {
            // Trailer section ends with an empty line.
            loop {
                line.clear();
                if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
                    return Ok(body);
                }
            }
        }
        if body.len() + size > max_body {
            bail!("response body exceeds the {max_body} byte limit");
        }
        let start = body.len();
        body.resize(start + size, 0);
        reader
            .read_exact(&mut body[start..])
            .context("read chunk")?;
        line.clear();
        reader.read_line(&mut line)?;
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
            other => bail!("unsupported Content-Encoding {other:?}"),
        };
    }
    Ok(body)
}

fn inflate(decoder: impl Read, max_body: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    decoder
        .take(max_body as u64 + 1)
        .read_to_end(&mut out)
        .context("decompress response body")?;
    if out.len() > max_body {
        bail!("decompressed body exceeds the {max_body} byte limit");
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
        assert!(Url::parse("https://пример.рф/").is_err());
    }

    #[test]
    fn joins_redirects() {
        let base = Url::parse("https://a.com/sub/abc?x").unwrap();
        assert_eq!(base.join("/new").unwrap().to_string(), "https://a.com/new");
        assert_eq!(
            base.join("def").unwrap().to_string(),
            "https://a.com/sub/def"
        );
        assert_eq!(
            base.join("//b.com/z").unwrap().to_string(),
            "https://b.com/z"
        );
        assert_eq!(
            base.join("http://c.com:81/").unwrap().to_string(),
            "http://c.com:81/"
        );
        assert_eq!(
            base.with_host("new.com").unwrap().to_string(),
            "https://new.com/sub/abc?x"
        );
    }

    fn parse(raw: &[u8]) -> Response {
        read_response(&mut Cursor::new(raw.to_vec()), "GET", 1 << 20).unwrap()
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
        assert!(read_response(&mut Cursor::new(Vec::new()), "GET", 10).is_err());
    }
}
