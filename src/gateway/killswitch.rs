//! Optional gateway kill switch (`gateway.kill_switch`).
//!
//! Without it the gateway fails open: while mihomo is down (first start, crash
//! restart) there is no TUN device, and traffic of this network namespace leaves
//! through the normal default route. With it, an `OUTPUT` firewall chain lets
//! out only what cannot leak:
//!
//! * traffic into the TUN device and over loopback;
//! * mihomo's own connections (`routing-mark`) and mihomyak's (`SO_MARK`, its
//!   DNS lookups included), so proxies and the subscription panel stay reachable;
//! * replies to incoming connections (published ports keep working);
//! * private, link-local and unique-local destinations (containers, LAN).
//!
//! Everything else is rejected, DNS included: while mihomo is down the apps'
//! names are not resolved at all rather than in the clear by an outside server
//! (a DNS server in the LAN is still reachable, as any private address is). The chain is installed atomically with
//! `iptables-restore`, which also covers nftables-backed `iptables` (the Docker
//! image ships `iptables-nft`). It stays while mihomyak runs and is removed on a
//! clean shutdown; after a crash it remains in place, which is the point.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// Firewall mark of connections allowed past the kill switch ("myk").
pub const MARK: u32 = 0x006d_796b;
/// TUN interface name mihomo is told to use, so the chain can allow it.
pub const TUN_DEVICE: &str = "mihomyak0";

const CHAIN: &str = "MIHOMYAK";
const LAN_V4: &[&str] = &[
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "169.254.0.0/16",
];
const LAN_V6: &[&str] = &["fc00::/7", "fe80::/10"];

#[derive(Clone, Copy)]
enum Family {
    V4,
    V6,
}

impl Family {
    fn tool(self) -> &'static str {
        match self {
            Family::V4 => "iptables",
            Family::V6 => "ip6tables",
        }
    }
}

/// Keeps the kill switch on; dropping it (clean shutdown) removes the rules.
pub struct KillSwitch {
    families: Vec<Family>,
}

pub fn enable() -> Result<KillSwitch> {
    install(Family::V4)
        .context("kill switch (IPv4): is iptables installed and CAP_NET_ADMIN granted?")?;
    let mut families = vec![Family::V4];
    match install(Family::V6) {
        Ok(()) => families.push(Family::V6),
        Err(e) if !ipv6_in_use() => {
            crate::debug!("no IPv6 kill switch (IPv6 is not configured here): {e:#}");
        }
        Err(e) => {
            return Err(e.context("kill switch (IPv6): IPv6 is configured but ip6tables failed"));
        }
    }
    crate::info!(
        "kill switch on: only {TUN_DEVICE}, mihomo, replies and private networks may leave this host/container"
    );
    Ok(KillSwitch { families })
}

impl Drop for KillSwitch {
    fn drop(&mut self) {
        for &family in &self.families {
            let tool = family.tool();
            let _ = run(tool, &["-D", "OUTPUT", "-j", CHAIN]);
            let _ = run(tool, &["-F", CHAIN]);
            let _ = run(tool, &["-X", CHAIN]);
        }
        crate::info!("kill switch off");
    }
}

/// `iptables-restore --noflush` input: declaring the chain resets it, and the
/// whole table commit is atomic, so there is never a window with a partial chain.
fn ruleset(family: Family, add_jump: bool) -> String {
    let mut out = format!("*filter\n:{CHAIN} - [0:0]\n");
    let mut rule = |spec: String| out.push_str(&format!("-A {CHAIN} {spec} -j RETURN\n"));
    rule("-o lo".into());
    rule(format!("-o {TUN_DEVICE}"));
    rule(format!("-m mark --mark {MARK:#x}"));
    rule("-m conntrack --ctdir REPLY".into());
    let lan = match family {
        Family::V4 => LAN_V4,
        Family::V6 => LAN_V6,
    };
    for net in lan {
        rule(format!("-d {net}"));
    }
    out.push_str(&format!("-A {CHAIN} -j REJECT\n"));
    if add_jump {
        out.push_str(&format!("-I OUTPUT 1 -j {CHAIN}\n"));
    }
    out.push_str("COMMIT\n");
    out
}

fn install(family: Family) -> Result<()> {
    let tool = family.tool();
    // A restarted supervisor finds its chain still hooked in: don't add it twice.
    let hooked = run(tool, &["-C", "OUTPUT", "-j", CHAIN]).is_ok();
    let restore = format!("{tool}-restore");
    let mut child = Command::new(&restore)
        .arg("--noflush")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("run {restore}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(ruleset(family, !hooked).as_bytes())?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "{restore} failed: {}",
            crate::util::sanitize(String::from_utf8_lossy(&output.stderr).trim())
        );
    }
    Ok(())
}

fn run(tool: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(tool)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("run {tool}"))?;
    if !output.status.success() {
        bail!(
            "{tool} {}: {}",
            args.join(" "),
            crate::util::sanitize(String::from_utf8_lossy(&output.stderr).trim())
        );
    }
    Ok(())
}

/// Whether any interface besides loopback has an IPv6 address.
fn ipv6_in_use() -> bool {
    std::fs::read_to_string("/proc/net/if_inet6").is_ok_and(|table| {
        table.lines().any(|line| {
            line.split_whitespace()
                .last()
                .is_some_and(|dev| dev != "lo")
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ruleset_allows_only_safe_paths() {
        let v4 = ruleset(Family::V4, true);
        assert!(v4.starts_with("*filter\n:MIHOMYAK - [0:0]\n"));
        assert!(v4.contains("-A MIHOMYAK -o mihomyak0 -j RETURN\n"));
        assert!(v4.contains("-A MIHOMYAK -m mark --mark 0x6d796b -j RETURN\n"));
        assert!(v4.contains("-A MIHOMYAK -d 172.16.0.0/12 -j RETURN\n"));
        assert!(!v4.contains("fc00::/7"));
        assert!(!v4.contains("--dport"), "no DNS or other port exceptions");
        // The reject comes last, the jump is added once, the commit closes it.
        let reject = v4.find("-A MIHOMYAK -j REJECT").unwrap();
        assert!(v4.rfind("-j RETURN").unwrap() < reject);
        assert!(v4.ends_with("-I OUTPUT 1 -j MIHOMYAK\nCOMMIT\n"));

        let v6 = ruleset(Family::V6, false);
        assert!(v6.contains("-d fc00::/7"));
        assert!(!v6.contains("10.0.0.0/8"));
        assert!(!v6.contains("-I OUTPUT"));
    }
}
