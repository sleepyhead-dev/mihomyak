//! Byte-exact comparison of mihomyak's subscription requests with requests
//! captured from the real clients (tests/fixtures/requests/README.md).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use mihomyak::config::Config;
use mihomyak::emulation::{ClientKind, Emulation};
use mihomyak::http::{Client, Url};
use mihomyak::identity::{Identity, OsRelease};

const UBUNTU_OS_RELEASE: &str = r#"PRETTY_NAME="Ubuntu 24.04.3 LTS"
NAME="Ubuntu"
VERSION_ID="24.04"
VERSION="24.04.3 LTS (Noble Numbat)"
VERSION_CODENAME=noble
ID=ubuntu
ID_LIKE=debian
"#;

/// Serves one canned response and returns the raw request head it received.
fn capture(emulation: &Emulation, response: &'static [u8]) -> (String, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            sock.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
        }
        sock.write_all(response).unwrap();
        String::from_utf8(head).unwrap()
    });
    let url = Url::parse(&format!("http://127.0.0.1:{port}/sub/abc")).unwrap();
    mihomyak::subscription::fetch(&Client::default(), emulation, &url).unwrap();
    (server.join().unwrap(), port)
}

fn emulation(kind: ClientKind) -> Emulation {
    emulation_with_id(kind, "0d0af05ee8fd4dc29275718f2ce4dff1")
}

fn emulation_with_id(kind: ClientKind, machine_id: &str) -> Emulation {
    let mut config = Config::default();
    config.subscription.client = kind;
    let identity = Identity {
        machine_id: machine_id.into(),
        os: OsRelease::from_raw(UBUNTU_OS_RELEASE),
        kernel_release: "6.8.0-45-generic".into(),
        hostname: "rpi-box".into(),
        locale: "en".into(),
    };
    Emulation::new(&config, identity).unwrap()
}

const OK: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\n\r\nproxies: []\n";

fn assert_golden(kind: ClientKind, fixture: &str) {
    let (sent, port) = capture(&emulation(kind), OK);
    let expected = fixture.replace("{PORT}", &port.to_string());
    assert_eq!(
        sent, expected,
        "request differs from the captured {kind} request"
    );
}

#[test]
fn flclashx_request_is_byte_exact() {
    assert_golden(
        ClientKind::FlClashX,
        include_str!("fixtures/requests/flclashx-0.4.2-linux.http"),
    );
}

#[test]
fn koala_request_is_byte_exact() {
    assert_golden(
        ClientKind::Koala,
        include_str!("fixtures/requests/koala-1.4.1-linux.http"),
    );
}

#[test]
fn happ_request_is_byte_exact() {
    let emulation = emulation_with_id(ClientKind::Happ, "11112222333344445555666677778888");
    let (sent, port) = capture(&emulation, OK);
    // The capture is from the x64 build on a day with an even Moscow date;
    // adapt the build id, CPU arch and daily marker to this run.
    let now = mihomyak::util::now_unix();
    let (build, arch) = if std::env::consts::ARCH == "aarch64" {
        ("2609151456", "arm64")
    } else {
        ("2609151457", std::env::consts::ARCH)
    };
    let marker = mihomyak::emulation::happ_day_marker(now);
    let expected = include_str!("fixtures/requests/happ-4.3.0-linux-x64.http")
        .replace("{PORT}", &port.to_string())
        .replace("2609151457698", &format!("{build}{marker}98"))
        .replace("rpi-box_x86_64", &format!("rpi-box_{arch}"));
    assert_eq!(
        sent, expected,
        "request differs from the captured Happ request"
    );
}

#[test]
fn redirects_keep_the_emulated_headers() {
    let target = TcpListener::bind("127.0.0.1:0").unwrap();
    let target_port = target.local_addr().unwrap().port();
    let target_thread = thread::spawn(move || {
        let (mut sock, _) = target.accept().unwrap();
        let mut buf = [0u8; 4096];
        let n = sock.read(&mut buf).unwrap();
        sock.write_all(OK).unwrap();
        String::from_utf8_lossy(&buf[..n]).into_owned()
    });
    let redirect: &'static [u8] = Box::leak(
        format!(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{target_port}/moved\r\nContent-Length: 0\r\n\r\n"
        )
        .into_bytes()
        .into_boxed_slice(),
    );
    let (first, _) = capture(&emulation(ClientKind::FlClashX), redirect);
    let second = target_thread.join().unwrap();
    assert!(first.starts_with("GET /sub/abc HTTP/1.1\r\n"));
    assert!(second.starts_with("GET /moved HTTP/1.1\r\n"));
    assert!(second.contains(&format!("host: 127.0.0.1:{target_port}\r\n")));
    assert!(second.contains("x-hwid: A3B522EAA6F7DD89\r\n"));
}
