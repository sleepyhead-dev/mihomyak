//! Name lookups past the gateway kill switch.
//!
//! The kill switch lets out only marked packets (`SO_MARK`), so while mihomo is
//! down the apps' DNS queries cannot leave in the clear. mihomyak still has to
//! find the panel then, so it asks the upstream servers itself over marked
//! sockets: plain DNS, A and AAAA, UDP with a TCP retry for truncated answers.
//! Names without a dot (containers, compose services) stay with the system
//! resolver: Docker answers them itself, without asking anyone outside.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};

const RESOLV_CONF: &str = "/etc/resolv.conf";
/// Used when the resolver configuration names no server reachable from here.
const FALLBACK: [IpAddr; 2] = [
    IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
    IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
];
const PER_SERVER: Duration = Duration::from_secs(3);
const TYPE_A: u16 = 1;
const TYPE_AAAA: u16 = 28;
const MAX_TCP_ANSWER: usize = 64 * 1024;

/// Whether a lookup of `host` must go around the system resolver.
pub(super) fn needs_marked_lookup(host: &str) -> bool {
    host.contains('.') && host.parse::<IpAddr>().is_err()
}

/// Addresses of `host` asked from the upstream servers over sockets carrying
/// `mark`: IPv4, or IPv6 when the name has no IPv4 address.
pub(super) fn resolve(
    host: &str,
    port: u16,
    mark: u32,
    deadline: Instant,
) -> Result<Vec<SocketAddr>> {
    let text = std::fs::read_to_string(RESOLV_CONF).unwrap_or_default();
    let servers = upstreams(&text);
    let mut found = Vec::new();
    let mut last_err = None;
    for qtype in [TYPE_A, TYPE_AAAA] {
        if !found.is_empty() {
            break;
        }
        for &server in &servers {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match lookup(
                SocketAddr::new(server, 53),
                host,
                qtype,
                Some(mark),
                PER_SERVER.min(left),
            ) {
                Ok(ips) => {
                    found.extend(ips.into_iter().map(|ip| SocketAddr::new(ip, port)));
                    break;
                }
                Err(e) => last_err = Some(e),
            }
        }
    }
    if found.is_empty() {
        return Err(match last_err {
            Some(e) => anyhow!(e).context(format!("resolve {host} (servers {servers:?})")),
            None => anyhow!("{host} has no addresses"),
        });
    }
    Ok(found)
}

/// Upstream servers reachable from this network namespace, from resolv.conf.
/// Docker's embedded resolver (`127.0.0.11`) lists its upstreams in a comment:
/// `# ExtServers: [1.1.1.1 host(192.168.0.1)]`. Loopback servers are skipped:
/// they forward from elsewhere, unmarked.
fn upstreams(resolv_conf: &str) -> Vec<IpAddr> {
    let docker = resolv_conf
        .lines()
        .find_map(|line| line.strip_prefix("# ExtServers:"))
        .map(|list| {
            list.trim()
                .trim_start_matches('[')
                .trim_end_matches(']')
                .split_whitespace()
                .map(|s| s.trim_start_matches("host(").trim_end_matches(')'))
                .collect::<Vec<_>>()
        });
    let listed = docker.unwrap_or_else(|| {
        resolv_conf
            .lines()
            .filter_map(|line| line.trim().strip_prefix("nameserver"))
            .map(str::trim)
            .collect()
    });
    let servers: Vec<IpAddr> = listed
        .into_iter()
        .filter_map(|s| {
            s.parse::<IpAddr>()
                .or_else(|_| s.parse::<SocketAddr>().map(|a| a.ip()))
                .ok()
        })
        .filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
        .collect();
    if servers.is_empty() {
        FALLBACK.to_vec()
    } else {
        servers
    }
}

/// One query of `qtype` for `host` to `server`; an empty list for NXDOMAIN.
fn lookup(
    server: SocketAddr,
    host: &str,
    qtype: u16,
    mark: Option<u32>,
    timeout: Duration,
) -> io::Result<Vec<IpAddr>> {
    let deadline = Instant::now() + timeout;
    let mut id = [0u8; 2];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut id)
        .map_err(|_| io::Error::other("no randomness for the DNS query id"))?;
    let query = encode_query(u16::from_be_bytes(id), host, qtype)?;

    let socket = match mark {
        Some(mark) => UdpSocket::from(super::marked_socket(&server, libc::SOCK_DGRAM, mark)?),
        None => UdpSocket::bind(if server.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        })?,
    };
    socket.connect(server)?;
    socket.send(&query)?;
    let mut buf = [0u8; 1500];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "DNS query timed out",
            ));
        }
        socket.set_read_timeout(Some(left))?;
        let n = socket.recv(&mut buf)?;
        match parse_answer(&buf[..n], &query, qtype) {
            // Not a reply to this query: keep waiting for it.
            Err(Answer::Mismatch) => continue,
            Err(Answer::Truncated) => return lookup_tcp(server, &query, qtype, mark, deadline),
            Err(Answer::Bad(e)) => return Err(io::Error::other(e)),
            Ok(ips) => return Ok(ips),
        }
    }
}

fn lookup_tcp(
    server: SocketAddr,
    query: &[u8],
    qtype: u16,
    mark: Option<u32>,
    deadline: Instant,
) -> io::Result<Vec<IpAddr>> {
    let left = super::remaining(deadline)?;
    let mut tcp = match mark {
        Some(mark) => super::connect_marked(&server, left, mark)?,
        None => std::net::TcpStream::connect_timeout(&server, left)?,
    };
    tcp.set_read_timeout(Some(super::remaining(deadline)?))?;
    tcp.set_write_timeout(Some(super::remaining(deadline)?))?;
    let len = u16::try_from(query.len()).map_err(io::Error::other)?;
    let mut framed = len.to_be_bytes().to_vec();
    framed.extend_from_slice(query);
    tcp.write_all(&framed)?;
    let mut len = [0u8; 2];
    tcp.read_exact(&mut len)?;
    let len = usize::from(u16::from_be_bytes(len)).min(MAX_TCP_ANSWER);
    let mut answer = vec![0u8; len];
    tcp.read_exact(&mut answer)?;
    parse_answer(&answer, query, qtype).map_err(|e| match e {
        Answer::Bad(e) => io::Error::other(e),
        Answer::Mismatch | Answer::Truncated => io::Error::other("unexpected DNS answer over TCP"),
    })
}

fn encode_query(id: u16, host: &str, qtype: u16) -> io::Result<Vec<u8>> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid host name {host:?}"),
        )
    };
    let name = host.strip_suffix('.').unwrap_or(host);
    if !name.is_ascii() || name.len() > 253 {
        return Err(invalid());
    }
    let mut out = Vec::with_capacity(18 + name.len());
    out.extend_from_slice(&id.to_be_bytes());
    // Recursion desired; one question.
    out.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
    for label in name.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(invalid());
        }
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    out.extend_from_slice(&qtype.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes());
    Ok(out)
}

enum Answer {
    /// Another id or question: a stray or forged packet.
    Mismatch,
    /// The server set TC: ask again over TCP.
    Truncated,
    Bad(String),
}

/// Addresses of type `qtype` in the answer section of a reply to `query`
/// (CNAME records are skipped: the recursive server includes their targets).
fn parse_answer(msg: &[u8], query: &[u8], qtype: u16) -> Result<Vec<IpAddr>, Answer> {
    let question = &query[12..];
    if msg.len() < 12 || msg[..2] != query[..2] || msg[2] & 0x80 == 0 {
        return Err(Answer::Mismatch);
    }
    // The question must be echoed (names compared case-insensitively).
    let echoed = msg.get(12..12 + question.len()).ok_or(Answer::Mismatch)?;
    if !echoed.eq_ignore_ascii_case(question) || u16::from_be_bytes([msg[4], msg[5]]) != 1 {
        return Err(Answer::Mismatch);
    }
    if msg[2] & 0x02 != 0 {
        return Err(Answer::Truncated);
    }
    match msg[3] & 0x0f {
        0 => {}
        3 => return Ok(Vec::new()),
        rcode => return Err(Answer::Bad(format!("DNS server answered rcode {rcode}"))),
    }
    let answers = u16::from_be_bytes([msg[6], msg[7]]);
    let bad = || Answer::Bad("malformed DNS answer".into());
    let mut pos = 12 + question.len();
    let mut ips = Vec::new();
    for _ in 0..answers {
        pos = skip_name(msg, pos).ok_or_else(bad)?;
        let fixed = msg.get(pos..pos + 10).ok_or_else(bad)?;
        let rtype = u16::from_be_bytes([fixed[0], fixed[1]]);
        let class = u16::from_be_bytes([fixed[2], fixed[3]]);
        let len = usize::from(u16::from_be_bytes([fixed[8], fixed[9]]));
        let data = msg.get(pos + 10..pos + 10 + len).ok_or_else(bad)?;
        pos += 10 + len;
        if class != 1 || rtype != qtype {
            continue;
        }
        match (rtype, <[u8; 4]>::try_from(data), <[u8; 16]>::try_from(data)) {
            (TYPE_A, Ok(v4), _) => ips.push(IpAddr::V4(Ipv4Addr::from(v4))),
            (TYPE_AAAA, _, Ok(v6)) => ips.push(IpAddr::V6(Ipv6Addr::from(v6))),
            _ => return Err(bad()),
        }
    }
    Ok(ips)
}

/// Position after the (possibly compressed) name at `pos`. Pointers are not
/// followed, so there are no loops to guard against.
fn skip_name(msg: &[u8], mut pos: usize) -> Option<usize> {
    loop {
        let len = *msg.get(pos)?;
        match len {
            0 => return Some(pos + 1),
            l if l & 0xc0 == 0xc0 => return msg.get(pos + 1).map(|_| pos + 2),
            l if l & 0xc0 == 0 => pos += 1 + usize::from(l),
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    const DOCKER: &str = "# Generated by Docker Engine.\nnameserver 127.0.0.11\noptions ndots:0\n\n\
                          # Based on host file: '/etc/resolv.conf' (internal resolver)\n";

    #[test]
    fn upstreams_come_from_docker_or_resolv_conf() {
        let explicit = format!("{DOCKER}# ExtServers: [9.9.9.9 1.0.0.1]\n# Overrides: []\n");
        let quad9 = IpAddr::from([9, 9, 9, 9]);
        assert_eq!(
            upstreams(&explicit),
            vec![quad9, IpAddr::from([1, 0, 0, 1])]
        );
        let router = format!("{DOCKER}# ExtServers: [host(192.168.0.1) host(127.0.0.53)]\n");
        assert_eq!(upstreams(&router), vec![IpAddr::from([192, 168, 0, 1])]);
        let plain = "nameserver 9.9.9.9\nnameserver 127.0.0.53\nnameserver 2606:4700::1111\n";
        assert_eq!(
            upstreams(plain),
            vec![
                IpAddr::from([9, 9, 9, 9]),
                "2606:4700::1111".parse().unwrap()
            ]
        );
        assert_eq!(upstreams("nameserver 127.0.0.53\n"), FALLBACK.to_vec());
        assert_eq!(upstreams(""), FALLBACK.to_vec());
    }

    #[test]
    fn only_dotted_names_bypass_the_system_resolver() {
        assert!(needs_marked_lookup("panel.example.com"));
        assert!(!needs_marked_lookup("squid"), "compose service names");
        assert!(!needs_marked_lookup("203.0.113.7"));
        assert!(!needs_marked_lookup("2001:db8::1"));
    }

    #[test]
    fn rejects_bad_names() {
        assert!(encode_query(1, "a..b", TYPE_A).is_err());
        assert!(encode_query(1, &format!("{}.com", "x".repeat(64)), TYPE_A).is_err());
        assert!(encode_query(1, "пример.рф", TYPE_A).is_err());
        assert_eq!(
            encode_query(1, "a.b.", TYPE_A).unwrap(),
            encode_query(1, "a.b", TYPE_A).unwrap()
        );
    }

    /// A reply to `query` with a CNAME and then `ips`, names compressed.
    fn reply(query: &[u8], flags: [u8; 2], ips: &[IpAddr]) -> Vec<u8> {
        let mut out = query[..2].to_vec();
        out.extend_from_slice(&flags);
        out.extend_from_slice(&[0, 1]);
        out.extend_from_slice(&(ips.len() as u16 + 1).to_be_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&query[12..]);
        // CNAME: pointer to the question name, target "c." + pointer.
        out.extend_from_slice(&[0xc0, 12, 0, 5, 0, 1, 0, 0, 0, 60, 0, 4, 1, b'c', 0xc0, 12]);
        for ip in ips {
            let (rtype, data) = match ip {
                IpAddr::V4(v4) => (TYPE_A, v4.octets().to_vec()),
                IpAddr::V6(v6) => (TYPE_AAAA, v6.octets().to_vec()),
            };
            out.extend_from_slice(&[0xc0, 12]);
            out.extend_from_slice(&rtype.to_be_bytes());
            out.extend_from_slice(&[0, 1, 0, 0, 0, 60]);
            out.extend_from_slice(&(data.len() as u16).to_be_bytes());
            out.extend_from_slice(&data);
        }
        out
    }

    #[test]
    fn parses_answers_and_rejects_strays() {
        let query = encode_query(0x1234, "panel.example.com", TYPE_A).unwrap();
        let ips = [
            IpAddr::from([203, 0, 113, 7]),
            IpAddr::from([203, 0, 113, 8]),
        ];
        let ok = reply(&query, [0x81, 0x80], &ips);
        assert_eq!(parse_answer(&ok, &query, TYPE_A).ok(), Some(ips.to_vec()));

        let mut other_id = ok.clone();
        other_id[1] ^= 1;
        assert!(matches!(
            parse_answer(&other_id, &query, TYPE_A),
            Err(Answer::Mismatch)
        ));
        let other_name = reply(
            &encode_query(0x1234, "evil.example.com", TYPE_A).unwrap(),
            [0x81, 0x80],
            &ips,
        );
        assert!(matches!(
            parse_answer(&other_name, &query, TYPE_A),
            Err(Answer::Mismatch)
        ));
        let truncated = reply(&query, [0x83, 0x80], &[]);
        assert!(matches!(
            parse_answer(&truncated, &query, TYPE_A),
            Err(Answer::Truncated)
        ));
        let nxdomain = reply(&query, [0x81, 0x83], &[]);
        assert_eq!(parse_answer(&nxdomain, &query, TYPE_A).ok(), Some(vec![]));
        let servfail = reply(&query, [0x81, 0x82], &[]);
        assert!(matches!(
            parse_answer(&servfail, &query, TYPE_A),
            Err(Answer::Bad(_))
        ));
        assert!(matches!(
            parse_answer(&ok[..ok.len() - 2], &query, TYPE_A),
            Err(Answer::Bad(_))
        ));
    }

    #[test]
    fn asks_a_server_over_udp_and_tcp() {
        let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        let tcp = std::net::TcpListener::bind(udp.local_addr().unwrap()).unwrap();
        let server = udp.local_addr().unwrap();
        let ip = IpAddr::from([203, 0, 113, 9]);
        let handle = thread::spawn(move || {
            let mut buf = [0u8; 512];
            // First a stray packet with a wrong id, then a truncated reply.
            let (n, peer) = udp.recv_from(&mut buf).unwrap();
            let mut stray = reply(&buf[..n], [0x81, 0x80], &[IpAddr::from([6, 6, 6, 6])]);
            stray[0] ^= 0xff;
            udp.send_to(&stray, peer).unwrap();
            udp.send_to(&reply(&buf[..n], [0x83, 0x80], &[]), peer)
                .unwrap();
            let (mut conn, _) = tcp.accept().unwrap();
            let mut len = [0u8; 2];
            conn.read_exact(&mut len).unwrap();
            let mut query = vec![0u8; usize::from(u16::from_be_bytes(len))];
            conn.read_exact(&mut query).unwrap();
            let answer = reply(&query, [0x81, 0x80], &[ip]);
            conn.write_all(&(answer.len() as u16).to_be_bytes())
                .unwrap();
            conn.write_all(&answer).unwrap();
        });
        let ips = lookup(
            server,
            "panel.example.com",
            TYPE_A,
            None,
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(ips, vec![ip]);
        handle.join().unwrap();
    }
}
