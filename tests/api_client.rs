//! Integration tests for the mihomo REST API client (`src/mihomo/api.rs`) against an
//! in-process fake HTTP server, following the pattern in
//! `tests/golden_requests.rs`.

mod support;

use std::net::TcpListener;

use mihomyak::mihomo::api::{Api, Rejected};
use support::{spawn_server, spawn_server_capturing_head, status_response};

fn api(port: u16, secret: &str) -> Api {
    Api::new(&format!("127.0.0.1:{port}"), secret).unwrap()
}

#[test]
fn rejected_status_carries_the_provider_message() {
    let body = br#"{"message":"proxy not found"}"#;
    let response = status_response(400, "Bad Request", body, &[]);
    let (port, server) = spawn_server(vec![response]);

    let err = api(port, "secret").mode().unwrap_err();
    let rejected = err
        .downcast_ref::<Rejected>()
        .unwrap_or_else(|| panic!("expected a Rejected error, got {err:#}"));
    assert_eq!(rejected.status, 400);
    assert_eq!(rejected.message, "proxy not found");

    server.join().unwrap();
}

#[test]
fn a_closed_port_is_a_transport_error_not_a_rejection() {
    // Bind, learn the port, then drop the listener: nothing answers on it.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let err = api(port, "secret").mode().unwrap_err();
    assert!(
        err.downcast_ref::<Rejected>().is_none(),
        "a connection failure must not look like a rejected request: {err:#}"
    );
}

#[test]
fn group_delay_of_a_fully_timed_out_group_is_empty() {
    let response = status_response(504, "Gateway Timeout", b"", &[]);
    let (port, server) = spawn_server(vec![response]);

    let delays = api(port, "")
        .group_delay("Auto", "https://example.com", 1000)
        .unwrap();
    assert!(delays.is_empty());

    server.join().unwrap();
}

#[test]
fn group_delay_parses_the_returned_map() {
    let body = br#"{"A":123}"#;
    let response = status_response(200, "OK", body, &[]);
    let (port, server) = spawn_server(vec![response]);

    let delays = api(port, "")
        .group_delay("Auto", "https://example.com", 1000)
        .unwrap();
    assert_eq!(delays.get("A"), Some(&123));

    server.join().unwrap();
}

#[test]
fn sends_bearer_authorization_header() {
    let response = status_response(200, "OK", b"{}", &[]);
    let (port, server) = spawn_server_capturing_head(response);

    api(port, "TopSecret123").version().unwrap();

    let head = server.join().unwrap();
    assert!(
        head.contains("Authorization: Bearer TopSecret123\r\n"),
        "request head did not carry the bearer token:\n{head}"
    );
}
