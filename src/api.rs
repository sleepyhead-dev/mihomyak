//! Client for mihomo's REST API (external-controller).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::config::Config;
use crate::http::{Client, Endpoint, Request, Response};
use crate::util::encode_path_segment;

/// Timeout for a single delay test, shared by the CLI and the TUI.
pub const DELAY_TIMEOUT_MS: u32 = 5000;

/// mihomo answered with a non-2xx status: it is up but refused the request
/// (as opposed to a transport error, where it may not be running at all).
#[derive(Debug)]
pub struct Rejected {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {} {}", self.status, self.message)
    }
}

impl std::error::Error for Rejected {}

pub struct Api {
    endpoint: Endpoint,
    host: String,
    secret: String,
    client: Client,
}

/// A proxy group as shown by clients: selector, url-test, fallback, load-balance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub name: String,
    pub kind: String,
    pub now: Option<String>,
    pub members: Vec<String>,
}

impl Group {
    pub fn selectable(&self) -> bool {
        self.kind == "Selector"
    }

    /// A selector the user picks from (GLOBAL only matters in global mode).
    pub fn is_user_selector(&self) -> bool {
        self.selectable() && self.name != "GLOBAL"
    }
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// Groups in config order (as listed by mihomo's GLOBAL group), GLOBAL last.
    pub groups: Vec<Group>,
    /// Last measured delay per proxy/group name (0 or missing = unknown/failed).
    pub delays: HashMap<String, u32>,
    pub kinds: HashMap<String, String>,
}

impl Api {
    /// API client for the configured controller (secret from config or data dir).
    pub fn from_config(config: &Config) -> Result<Self> {
        let secret = match &config.core.secret {
            Some(secret) => secret.clone(),
            None => crate::store::Store::open(&config.data_dir)?.secret()?,
        };
        Self::new(&config.core.controller, &secret)
    }

    /// `controller` is `host:port` or `unix:/path` (same syntax as `core.controller`).
    pub fn new(controller: &str, secret: &str) -> Result<Self> {
        let (endpoint, host) = match controller.strip_prefix("unix:") {
            Some(path) => (Endpoint::Unix(PathBuf::from(path)), "localhost".to_owned()),
            None => {
                let (host, port) = controller
                    .rsplit_once(':')
                    .with_context(|| format!("controller {controller:?} must be host:port"))?;
                let port: u16 = port
                    .parse()
                    .with_context(|| format!("bad controller port {port:?}"))?;
                // A wildcard listen address is reachable via loopback.
                let host = match host.trim_matches(['[', ']']) {
                    "" | "0.0.0.0" | "*" => "127.0.0.1",
                    "::" => "::1",
                    h => h,
                }
                .to_owned();
                let host_header = if host.contains(':') {
                    format!("[{host}]:{port}")
                } else {
                    format!("{host}:{port}")
                };
                (
                    Endpoint::Tcp {
                        host,
                        port,
                        tls: false,
                    },
                    host_header,
                )
            }
        };
        let client = Client {
            connect_timeout: Duration::from_secs(3),
            io_timeout: Duration::from_secs(30),
            total_timeout: Duration::from_secs(60),
            proxy: None,
            max_body: 16 * 1024 * 1024,
            // Loopback or a unix socket: never subject to the kill switch.
            mark: None,
        };
        Ok(Self {
            endpoint,
            host,
            secret: secret.to_owned(),
            client,
        })
    }

    fn call(&self, method: &str, target: &str, body: Option<&Value>) -> Result<Response> {
        let body = body.map(|b| b.to_string().into_bytes()).unwrap_or_default();
        let mut headers = vec![("Host".to_owned(), self.host.clone())];
        if !self.secret.is_empty() {
            headers.push(("Authorization".into(), format!("Bearer {}", self.secret)));
        }
        if !body.is_empty() {
            headers.push(("Content-Type".into(), "application/json".into()));
        }
        headers.push(("Connection".into(), "close".into()));
        let response = self.client.send(
            &self.endpoint,
            &Request {
                method,
                target,
                headers: &headers,
                body: &body,
            },
        )?;
        if !(200..300).contains(&response.status) {
            let message = serde_json::from_slice::<Value>(&response.body)
                .ok()
                .and_then(|v| v["message"].as_str().map(str::to_owned))
                .unwrap_or_else(|| String::from_utf8_lossy(&response.body).into_owned());
            let message: String = crate::util::sanitize(message.trim())
                .chars()
                .take(500)
                .collect();
            let path = target.split('?').next().unwrap_or_default();
            return Err(anyhow::Error::new(Rejected {
                status: response.status,
                message,
            })
            .context(format!("mihomo API {method} {path}")));
        }
        Ok(response)
    }

    fn json(&self, method: &str, target: &str, body: Option<&Value>) -> Result<Value> {
        let response = self.call(method, target, body)?;
        if response.body.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&response.body).context("mihomo API returned invalid JSON")
    }

    pub fn version(&self) -> Result<String> {
        let v = self.json("GET", "/version", None)?;
        Ok(v["version"].as_str().unwrap_or("unknown").to_owned())
    }

    pub fn mode(&self) -> Result<String> {
        let v = self.json("GET", "/configs", None)?;
        Ok(v["mode"].as_str().unwrap_or("unknown").to_owned())
    }

    pub fn set_mode(&self, mode: &str) -> Result<()> {
        self.call("PATCH", "/configs", Some(&json!({ "mode": mode })))?;
        Ok(())
    }

    /// Hot-reloads a config file (must live inside mihomo's home directory).
    /// A [`Rejected`] error means mihomo is running but refused the config.
    pub fn reload(&self, path: &std::path::Path) -> Result<()> {
        let body = json!({ "path": path.to_string_lossy() });
        self.call("PUT", "/configs?force=true", Some(&body))?;
        Ok(())
    }

    pub fn snapshot(&self) -> Result<Snapshot> {
        Ok(parse_snapshot(&self.json("GET", "/proxies", None)?))
    }

    pub fn select(&self, group: &str, proxy: &str) -> Result<()> {
        let target = format!("/proxies/{}", encode_path_segment(group));
        self.call("PUT", &target, Some(&json!({ "name": proxy })))?;
        Ok(())
    }

    /// Tests every member of a group; returns name → delay (ms). Failed proxies are
    /// absent; when all fail mihomo answers 504, which is an empty map here.
    pub fn group_delay(
        &self,
        group: &str,
        url: &str,
        timeout_ms: u32,
    ) -> Result<HashMap<String, u32>> {
        let target = format!(
            "/group/{}/delay?url={}&timeout={timeout_ms}",
            encode_path_segment(group),
            encode_path_segment(url)
        );
        let v = match self.json("GET", &target, None) {
            Err(e)
                if e.downcast_ref::<Rejected>()
                    .is_some_and(|r| r.status == 504) =>
            {
                return Ok(HashMap::new());
            }
            other => other?,
        };
        Ok(v.as_object()
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| Some((k.clone(), u32::try_from(v.as_u64()?).ok()?)))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// (upload total, download total, active connections) since core start.
    pub fn traffic(&self) -> Result<(u64, u64, usize)> {
        let v = self.json("GET", "/connections", None)?;
        Ok((
            v["uploadTotal"].as_u64().unwrap_or(0),
            v["downloadTotal"].as_u64().unwrap_or(0),
            v["connections"].as_array().map_or(0, Vec::len),
        ))
    }
}

fn parse_snapshot(v: &Value) -> Snapshot {
    let Some(proxies) = v["proxies"].as_object() else {
        return Snapshot::default();
    };
    let group_of = |name: &str| -> Option<Group> {
        let p = proxies.get(name)?;
        let members: Vec<String> = p["all"]
            .as_array()?
            .iter()
            .filter_map(|m| m.as_str().map(str::to_owned))
            .collect();
        (!p["hidden"].as_bool().unwrap_or(false)).then(|| Group {
            name: name.to_owned(),
            kind: p["type"].as_str().unwrap_or_default().to_owned(),
            now: p["now"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            members,
        })
    };
    // GLOBAL lists every group and proxy in config order.
    let mut groups: Vec<Group> = proxies
        .get("GLOBAL")
        .and_then(|g| g["all"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|m| m.as_str())
        .filter(|name| *name != "GLOBAL")
        .filter_map(group_of)
        .collect();
    if let Some(global) = group_of("GLOBAL") {
        groups.push(global);
    }
    let delays = proxies
        .iter()
        .filter_map(|(name, p)| {
            let delay = p["history"].as_array()?.last()?["delay"].as_u64()?;
            Some((name.clone(), u32::try_from(delay).ok()?))
        })
        .collect();
    let kinds = proxies
        .iter()
        .map(|(name, p)| {
            (
                name.clone(),
                p["type"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    Snapshot {
        groups,
        delays,
        kinds,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_groups_like_the_config() {
        let v = json!({"proxies": {
            "GLOBAL": {"type": "Selector", "now": "PROXY", "all": ["PROXY", "AUTO", "Hidden", "NL", "DIRECT"]},
            "AUTO": {"type": "URLTest", "now": "NL", "all": ["NL"], "history": []},
            "PROXY": {"type": "Selector", "now": "AUTO", "all": ["AUTO", "NL"]},
            "Hidden": {"type": "Selector", "hidden": true, "all": ["NL"]},
            "NL": {"type": "Vless", "history": [{"delay": 120}, {"delay": 87}]},
            "DIRECT": {"type": "Direct", "history": []}
        }});
        let s = parse_snapshot(&v);
        let names: Vec<_> = s.groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, ["PROXY", "AUTO", "GLOBAL"]);
        assert!(s.groups[0].selectable() && !s.groups[1].selectable());
        assert_eq!(s.groups[1].now.as_deref(), Some("NL"));
        assert_eq!(s.delays.get("NL"), Some(&87));
        assert_eq!(s.kinds["NL"], "Vless");
    }

    #[test]
    fn group_selector_helpers() {
        let selector = Group {
            name: "PROXY".to_owned(),
            kind: "Selector".to_owned(),
            now: None,
            members: vec![],
        };
        assert!(selector.selectable());
        assert!(selector.is_user_selector());

        let global = Group {
            name: "GLOBAL".to_owned(),
            ..selector.clone()
        };
        assert!(global.selectable());
        assert!(
            !global.is_user_selector(),
            "GLOBAL only matters in global mode"
        );

        let url_test = Group {
            kind: "URLTest".to_owned(),
            ..selector
        };
        assert!(!url_test.selectable());
        assert!(!url_test.is_user_selector());
    }

    #[test]
    fn controller_addresses() {
        let api = Api::new("0.0.0.0:9090", "").unwrap();
        assert_eq!(api.host, "127.0.0.1:9090");
        let api = Api::new("[::]:9090", "").unwrap();
        assert_eq!(api.host, "[::1]:9090");
        assert!(matches!(
            Api::new("unix:/tmp/m.sock", "").unwrap().endpoint,
            Endpoint::Unix(_)
        ));
        assert!(Api::new("nonsense", "").is_err());
    }
}
