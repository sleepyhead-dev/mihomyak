//! Integration tests for the subscription update pipeline (`src/updater.rs`)
//! against in-process fake HTTP servers, following the pattern in
//! `tests/golden_requests.rs`.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use mihomyak::config::Config;
use mihomyak::emulation::ClientKind;
use mihomyak::subscription::Problem;
use mihomyak::updater::{Outcome, Updater};

const GOOD_BODY: &[u8] = include_bytes!("fixtures/subscriptions/remnawave-mihomo.yaml");
/// Remnawave-style stub: a single placeholder proxy, no real servers.
const STUB_BODY: &[u8] =
    b"proxies:\n  - {name: Limit of devices reached, type: vless, server: 0.0.0.0, port: 1}\n";

/// Serves `responses` in order, one per accepted connection, then exits.
/// Read/write timeouts on every socket keep a broken test from hanging CI.
fn spawn_server(responses: Vec<Vec<u8>>) -> (u16, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        for response in responses {
            let (mut sock, _) = listener.accept().unwrap();
            sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            sock.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                sock.read_exact(&mut byte).unwrap();
                head.push(byte[0]);
            }
            sock.write_all(&response).unwrap();
        }
    });
    (port, handle)
}

/// A `200 OK` response with a body and extra headers (e.g. `subscription-userinfo`).
fn ok_response(body: &[u8], extra_headers: &[(&str, &str)]) -> Vec<u8> {
    status_response(200, "OK", body, extra_headers)
}

fn status_response(
    status: u16,
    reason: &str,
    body: &[u8],
    extra_headers: &[(&str, &str)],
) -> Vec<u8> {
    let mut head = format!("HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\n", body.len());
    for (name, value) in extra_headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut wire = head.into_bytes();
    wire.extend_from_slice(body);
    wire
}

/// A default config pointed at `port`, with `mihomo -t` validation guaranteed to be
/// skipped (nonexistent binary path): `Updater::validate` logs a warning and lets the
/// config through, so tests are deterministic without depending on a mihomo binary
/// or `PATH` in CI.
fn test_config(dir: &Path, port: u16) -> Config {
    let mut config = Config::default();
    config.data_dir = dir.to_path_buf();
    config.subscription.url = Some(format!("http://127.0.0.1:{port}/sub/abc"));
    config.subscription.client = ClientKind::FlClashX;
    config.core.bin = dir.join("no-such-mihomo-binary");
    config
}

#[test]
fn good_subscription_is_applied() {
    let dir = tempfile::tempdir().unwrap();
    let response = ok_response(
        GOOD_BODY,
        &[
            (
                "subscription-userinfo",
                "upload=100; download=200; total=1000000000; expire=1767225600",
            ),
            ("profile-update-interval", "12"),
        ],
    );
    let (port, server) = spawn_server(vec![response]);
    let updater = Updater::new(test_config(dir.path(), port)).unwrap();

    match updater.update().unwrap() {
        Outcome::Applied { changed, info, proxies } => {
            assert!(changed, "first update must write a new config");
            assert_eq!(proxies, 1);
            assert_eq!(info.usage.unwrap().total, 1_000_000_000);
            assert_eq!(info.update_interval, Some(Duration::from_secs(12 * 3600)));
        }
        Outcome::Kept(problem) => panic!("expected the subscription to be applied: {problem}"),
    }

    let config_path = updater.store.mihomo_config();
    assert!(config_path.exists(), "config.yaml must exist in the store's mihomo dir");
    let meta = updater.store.load_meta().expect("meta.json must be written");
    assert_eq!(meta.format, "mihomo");
    assert_eq!(meta.proxies, 1);
    assert!(meta.last_error.is_none());

    server.join().unwrap();
}

#[test]
fn identical_body_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let response = ok_response(GOOD_BODY, &[("profile-update-interval", "12")]);
    let (port, server) = spawn_server(vec![response.clone(), response]);
    let updater = Updater::new(test_config(dir.path(), port)).unwrap();

    match updater.update().unwrap() {
        Outcome::Applied { changed, .. } => assert!(changed),
        Outcome::Kept(problem) => panic!("expected the first fetch to apply: {problem}"),
    }
    match updater.update().unwrap() {
        Outcome::Applied { changed, .. } => {
            assert!(!changed, "the same body must not be rewritten");
        }
        Outcome::Kept(problem) => panic!("expected the second fetch to apply unchanged: {problem}"),
    }

    server.join().unwrap();
}

#[test]
fn server_error_keeps_previous_config() {
    let dir = tempfile::tempdir().unwrap();
    let good = ok_response(GOOD_BODY, &[]);
    let error = status_response(500, "Internal Server Error", b"boom", &[]);
    let (port, server) = spawn_server(vec![good, error]);
    let updater = Updater::new(test_config(dir.path(), port)).unwrap();

    assert!(matches!(updater.update().unwrap(), Outcome::Applied { .. }));
    let before = std::fs::read(updater.store.mihomo_config()).unwrap();

    match updater.update().unwrap() {
        Outcome::Kept(problem) => assert!(matches!(problem, Problem::Http(_)), "{problem:?}"),
        Outcome::Applied { .. } => panic!("an HTTP 500 must not be applied"),
    }
    let after = std::fs::read(updater.store.mihomo_config()).unwrap();
    assert_eq!(before, after, "config.yaml must be untouched after a failed update");
    let meta = updater.store.load_meta().unwrap();
    assert!(meta.last_error.is_some());

    server.join().unwrap();
}

#[test]
fn stub_with_only_placeholder_proxies_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let good = ok_response(GOOD_BODY, &[]);
    let stub = ok_response(STUB_BODY, &[]);
    let (port, server) = spawn_server(vec![good, stub]);
    let updater = Updater::new(test_config(dir.path(), port)).unwrap();

    assert!(matches!(updater.update().unwrap(), Outcome::Applied { .. }));
    let before = std::fs::read(updater.store.mihomo_config()).unwrap();

    match updater.update().unwrap() {
        Outcome::Kept(problem) => assert!(matches!(problem, Problem::Stub(_)), "{problem:?}"),
        Outcome::Applied { .. } => panic!("a placeholder-only stub must not be applied"),
    }
    let after = std::fs::read(updater.store.mihomo_config()).unwrap();
    assert_eq!(before, after);

    server.join().unwrap();
}

#[test]
fn hwid_max_devices_reached_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let good = ok_response(GOOD_BODY, &[]);
    let refused = ok_response(STUB_BODY, &[("x-hwid-max-devices-reached", "true")]);
    let (port, server) = spawn_server(vec![good, refused]);
    let updater = Updater::new(test_config(dir.path(), port)).unwrap();

    assert!(matches!(updater.update().unwrap(), Outcome::Applied { .. }));
    let before = std::fs::read(updater.store.mihomo_config()).unwrap();

    match updater.update().unwrap() {
        Outcome::Kept(problem) => {
            assert!(matches!(problem, Problem::Refused(_)), "{problem:?}");
            assert!(problem.message().contains("device limit"));
        }
        Outcome::Applied { .. } => panic!("a device-limit refusal must not be applied"),
    }
    let after = std::fs::read(updater.store.mihomo_config()).unwrap();
    assert_eq!(before, after);

    server.join().unwrap();
}

#[test]
fn backup_is_kept_after_a_second_different_update() {
    let dir = tempfile::tempdir().unwrap();
    let second_body = String::from_utf8_lossy(GOOD_BODY)
        .replace("nl.example.com", "de.example.com")
        .into_bytes();
    let first = ok_response(GOOD_BODY, &[]);
    let second = ok_response(&second_body, &[]);
    let (port, server) = spawn_server(vec![first, second]);
    let updater = Updater::new(test_config(dir.path(), port)).unwrap();

    assert!(matches!(updater.update().unwrap(), Outcome::Applied { changed: true, .. }));
    let config_path = updater.store.mihomo_config();
    let first_config = std::fs::read(&config_path).unwrap();
    let prev_path = config_path.with_file_name("config.yaml.prev");
    assert!(!prev_path.exists(), "nothing to back up before the first write");

    assert!(matches!(updater.update().unwrap(), Outcome::Applied { changed: true, .. }));
    let second_config = std::fs::read(&config_path).unwrap();
    assert_ne!(first_config, second_config, "the second update must rewrite the config");
    assert!(prev_path.exists(), "the first config must be backed up as .prev");
    let backed_up = std::fs::read(&prev_path).unwrap();
    assert_eq!(backed_up, first_config);

    server.join().unwrap();
}

#[test]
fn flclashx_newdomain_over_http_is_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    // Docs: FlClashX only honours `flclashx-newdomain` from a usable https response.
    let response = ok_response(GOOD_BODY, &[("flclashx-newdomain", "newhost.example.com")]);
    let (port, server) = spawn_server(vec![response]);
    let config = test_config(dir.path(), port);
    let original_url = config.subscription_url().unwrap().to_owned();
    let updater = Updater::new(config).unwrap();

    assert!(matches!(updater.update().unwrap(), Outcome::Applied { .. }));

    let meta = updater.store.load_meta().unwrap();
    assert!(meta.url_override.is_none(), "a plain-http newdomain header must be ignored");
    assert_eq!(updater.url().unwrap().to_string(), original_url);

    server.join().unwrap();
}
