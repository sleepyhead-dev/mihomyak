//! Shared fake HTTP server helpers for integration tests, used by `tests/api_client.rs`
//! and `tests/updater_pipeline.rs`. Follows the pattern in `tests/golden_requests.rs`.
//!
//! Each integration test file is compiled as its own crate and includes this module
//! separately, so a helper only one file uses is marked `#[allow(dead_code)]` on the
//! item rather than crate-wide.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Serves `responses` in order, one per accepted connection, then exits.
/// Read/write timeouts on every socket keep a broken test from hanging CI.
pub fn spawn_server(responses: Vec<Vec<u8>>) -> (u16, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        for response in responses {
            let (mut sock, _) = listener.accept().unwrap();
            sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            sock.set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            read_request_head(&mut sock);
            sock.write_all(&response).unwrap();
        }
    });
    (port, handle)
}

/// Like `spawn_server`, but for a single connection: returns the raw request head it
/// received instead of discarding it.
#[allow(dead_code)]
pub fn spawn_server_capturing_head(response: Vec<u8>) -> (u16, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        sock.set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let head = read_request_head(&mut sock);
        sock.write_all(&response).unwrap();
        String::from_utf8(head).unwrap()
    });
    (port, handle)
}

/// Reads and discards a request head byte by byte until the terminating CRLFCRLF.
fn read_request_head(sock: &mut impl Read) -> Vec<u8> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        sock.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
    }
    head
}

/// A `200 OK` response with a body and extra headers (e.g. `subscription-userinfo`).
#[allow(dead_code)]
pub fn ok_response(body: &[u8], extra_headers: &[(&str, &str)]) -> Vec<u8> {
    status_response(200, "OK", body, extra_headers)
}

/// A response with an explicit status line, body, and extra headers.
pub fn status_response(
    status: u16,
    reason: &str,
    body: &[u8],
    extra_headers: &[(&str, &str)],
) -> Vec<u8> {
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in extra_headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut wire = head.into_bytes();
    wire.extend_from_slice(body);
    wire
}
