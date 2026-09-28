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

mod dns;
mod response;
mod url;

use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

pub use response::Response;
pub(crate) use response::inflate;
pub use url::{Scheme, Url};

/// Where to open the connection.
#[derive(Clone, Debug)]
pub enum Endpoint {
    Tcp { host: String, port: u16, tls: bool },
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
        let mut response = response::read_response(&mut stream, req.method, self.max_body)?;
        response.peer = peer;
        Ok(response)
    }

    fn connect(&self, endpoint: &Endpoint, deadline: Instant) -> Result<(Inner, Option<IpAddr>)> {
        match endpoint {
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
        let addrs: Vec<SocketAddr> = match self.mark {
            // The kill switch keeps DNS in: ask past it (dns.rs).
            Some(mark) if dns::needs_marked_lookup(host) => {
                dns::resolve(host, port, mark, deadline)?
            }
            _ => (host, port)
                .to_socket_addrs()
                .with_context(|| format!("resolve {host}"))?
                .collect(),
        };
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
            if head.len() > response::MAX_HEAD_BYTES || stream.read(&mut byte)? == 0 {
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

fn check(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

/// A new socket of `kind` (`SOCK_STREAM`, `SOCK_DGRAM`) for `addr`'s family with
/// `SO_MARK` set, so the gateway kill switch lets its packets out.
fn marked_socket(
    addr: &SocketAddr,
    kind: libc::c_int,
    mark: u32,
) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    let family = if addr.is_ipv4() {
        libc::AF_INET
    } else {
        libc::AF_INET6
    };
    // SAFETY: plain socket syscalls; the descriptor is owned by `fd` at once and
    // the mark is passed with its exact size.
    unsafe {
        let raw = check(libc::socket(family, kind | libc::SOCK_CLOEXEC, 0))?;
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
        Ok(fd)
    }
}

/// `TcpStream::connect_timeout` with `SO_MARK` set before the SYN leaves: std
/// cannot configure a socket before connecting, so this is done with libc.
fn connect_marked(addr: &SocketAddr, timeout: Duration, mark: u32) -> io::Result<TcpStream> {
    use std::os::fd::AsRawFd;

    let fd = marked_socket(addr, libc::SOCK_STREAM | libc::SOCK_NONBLOCK, mark)?;
    // SAFETY: plain socket syscalls on a descriptor owned by `fd`; the sockaddr
    // structs are fully initialised (zeroed, then every relevant field set) and
    // passed with their exact sizes.
    unsafe {
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
        };
        result.map_err(|e| self.timed_out(e))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.arm()?;
        let result = match &mut self.inner {
            Inner::Tcp(s) => s.flush(),
            Inner::Tls(s) => s.flush(),
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// An in-memory-like connected pair, for tests that only need a stream to
    /// read and write on (not real network conditions).
    fn tcp_pair() -> (TcpStream, TcpStream) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    #[test]
    fn total_deadline_stops_a_trickling_server() {
        let (client, mut server) = tcp_pair();
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
            inner: Inner::Tcp(client),
            deadline: Instant::now() + Duration::from_millis(200),
            io_timeout: Duration::from_secs(5),
        };
        let started = Instant::now();
        assert!(response::read_response(&mut stream, "GET", 1024).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(stream);
        writer.join().unwrap();
    }

    #[test]
    fn timeouts_name_what_ran_out() {
        let read = |deadline, io_timeout| {
            let (client, _server) = tcp_pair();
            let mut stream = Stream {
                inner: Inner::Tcp(client),
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
