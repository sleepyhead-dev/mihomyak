//! Validates generated configs with a real mihomo binary (`mihomo -t`).
//!
//! Skipped unless `MIHOMYAK_TEST_MIHOMO` points at a mihomo executable
//! (CI downloads one; locally: `mihomyak core install --dest /tmp/mihomo`).

use std::path::PathBuf;
use std::process::Command;

use mihomyak::config::Config;
use mihomyak::profile::Params;
use mihomyak::subscription::body;

fn mihomo() -> Option<PathBuf> {
    let bin = std::env::var_os("MIHOMYAK_TEST_MIHOMO").map(PathBuf::from);
    if bin.is_none() {
        eprintln!("MIHOMYAK_TEST_MIHOMO not set: skipping real mihomo validation");
    }
    bin
}

fn validate(name: &str, body: &[u8], gateway: bool) {
    let mut config = Config::default();
    config.gateway.enable = gateway;
    validate_with(name, body, &config);
}

fn validate_with(name: &str, body: &[u8], config: &Config) {
    // Parsing and building run everywhere; only the mihomo check needs the binary.
    let content = body::parse(body, None).unwrap_or_else(|e| panic!("{name}: {e}"));
    assert!(content.notes.is_empty(), "{name}: {:?}", content.notes);
    let params = Params {
        secret: "secret",
        mode: "rule",
        panel_hosts: &["sub.example.com".to_owned()],
        panel_ips: &[
            "203.0.113.7".parse().unwrap(),
            "2001:db8::7".parse().unwrap(),
        ],
    };
    let built = mihomyak::profile::build(&content, config, &params)
        .unwrap_or_else(|e| panic!("{name}: {e:#}"));
    let Some(bin) = mihomo() else { return };
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("config.yaml"), &built.config_yaml).unwrap();
    if let Some(provider) = &built.provider {
        let path = home.path().join(mihomyak::profile::PROVIDER_FILE);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, provider).unwrap();
    }
    let out = Command::new(&bin)
        .arg("-t")
        .arg("-d")
        .arg(home.path())
        .arg("-f")
        .arg(home.path().join("config.yaml"))
        .output()
        .unwrap();
    let log =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && log.contains("test is successful"),
        "{name}: mihomo rejected the generated config:\n{log}\n---\n{}",
        built.config_yaml
    );
}

#[test]
fn remnawave_mihomo_yaml() {
    validate(
        "remnawave-mihomo",
        include_bytes!("fixtures/subscriptions/remnawave-mihomo.yaml"),
        false,
    );
}

#[test]
fn remnawave_mihomo_yaml_gateway() {
    validate(
        "remnawave-mihomo+gateway",
        include_bytes!("fixtures/subscriptions/remnawave-mihomo.yaml"),
        true,
    );
}

#[test]
fn xray_json_conversion() {
    validate(
        "remnawave-xray",
        include_bytes!("fixtures/subscriptions/remnawave-xray.json"),
        false,
    );
}

#[test]
fn base64_links() {
    validate(
        "links",
        include_bytes!("fixtures/subscriptions/links.b64.txt"),
        false,
    );
}

const NODE_SETTINGS: &str = r#"
[filter]
exclude = ["*SS*"]

[[groups]]
name = "Auto EU"
type = "fallback"
nodes = ["*DE*", "*NL*"]
default = true

[[groups]]
name = "Fastest"
type = "url-test"

[rules]
prepend = ["DOMAIN-SUFFIX,lan,DIRECT", "DOMAIN-SUFFIX,example.org,Fastest"]
"#;

#[test]
fn filters_groups_and_rules_on_xray_json() {
    let config: Config = toml::from_str(NODE_SETTINGS).unwrap();
    validate_with(
        "xray+nodes",
        include_bytes!("fixtures/subscriptions/remnawave-xray.json"),
        &config,
    );
}

#[test]
fn filters_groups_and_rules_on_links() {
    let config: Config = toml::from_str(NODE_SETTINGS).unwrap();
    validate_with(
        "links+nodes",
        include_bytes!("fixtures/subscriptions/links.b64.txt"),
        &config,
    );
}
