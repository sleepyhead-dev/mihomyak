//! Reading an HTTP/1.1 response: the status line, headers, body framing
//! (`Content-Length`, chunked, trailers, close-delimited) and decompression.
//!
//! The server is untrusted: every line, header block, chunk, trailer section and the
//! decoded body are bounded.

use std::io::{self, BufRead, BufReader, Read};
use std::net::IpAddr;

use anyhow::{Context, Result, anyhow, bail};

pub(super) const MAX_HEAD_BYTES: usize = 64 * 1024;
const MAX_LINE_BYTES: usize = 16 * 1024;
const MAX_TRAILERS: usize = 64;
const MAX_INTERIM_RESPONSES: usize = 8;

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

pub(super) fn read_response(
    stream: &mut impl Read,
    method: &str,
    max_body: usize,
) -> Result<Response> {
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
        use std::io::Write;

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
}
