# Changelog

Format: [Keep a Changelog](https://keepachangelog.com/). Versions follow SemVer.

## [Unreleased]

### Added
- **Happ for Windows and Android.** `platform = "windows" | "android"`
  (`MIHOMYAK_PLATFORM`, installer `--platform`) makes the `happ` client send
  exactly what the real apps send: requests of Happ for Windows 4.2.1–4.4.8
  (x64 and arm64) and Happ for Android 4.4.1/4.6.0 were captured on GitHub
  runners and an Android emulator (`.github/workflows/capture.yml`). Many
  providers refuse Linux but accept these. The Windows MachineGuid and
  computer name and the Android ID are derived from the seed; `device.model`
  and `device.os_version` change the reported phone and OS version.
  `fetch`/`identity` take `--platform`. The installer asks for it when Happ is
  chosen.

### Removed
- `subscription.accept_stub` / `MIHOMYAK_ACCEPT_STUB`: a provider stub is never
  applied now, only ever refused.
- The `custom` client kind. Use `flclashx`, `koala` or `happ`; the
  `subscription.user_agent` override still works with any of them.
- `device.send_headers`: the `x-hwid`/`x-device-*` headers are always sent.
- `device.os_name`, `device.os_version`, `device.os_pretty_name` overrides.
  `device.os_release` (the file path) is unchanged.
- The unix-socket mihomo controller (`core.controller = "unix:/path"`).
  `core.controller` is always `host:port` now.
- `gateway.dns_listen`: mihomo's DNS server always listens on the fixed
  `127.0.0.1:1053`.

## [0.3.0] - 2026-09-28

### Added
- One-command installer: `curl -fsSL …/releases/latest/download/install.sh | sh`.
  It asks for the link, the mode (Docker gateway, Docker proxy or a systemd
  service) and a device seed (any word, or a random one), installs into
  `/opt/mihomyak`, waits until mihomyak is healthy, enables autostart and adds a
  `mihomyak` command on the host (`status`, `tui`, `logs`, `upgrade`,
  `uninstall`). `--yes` with flags installs without questions; `update` and
  `uninstall` use the same script. Messages follow the system language (Russian
  or English). Tested in CI on amd64 and arm64 for all three modes; releases
  also run the published one-liner.
- The compose files take the image from `MIHOMYAK_IMAGE` (to pin a version).

### Changed
- The `ru-direct` preset is off by default again: all traffic goes through the
  VPN unless `MIHOMYAK_RULES_PRESETS=ru-direct` is set.

## [0.2.0] - 2026-09-27

Tested by hand on a Raspberry Pi 3B+ (arm64) and automatically on every change
with a gateway stand on amd64 and arm64.

### Upgrading from 0.1.0
- The compose files moved to `deploy/docker/` and now pin the project name
  `mihomyak`, so the data volume is `mihomyak_data`. The old one was named after
  the directory (usually `deploy_mihomyak-data`) and holds your device id: set
  `MIHOMYAK_DEVICE_SEED` (a new device once, then stable forever) or copy the
  old id with `docker run --rm -v deploy_mihomyak-data:/data alpine cat /data/machine-id`
  into `MIHOMYAK_MACHINE_ID`.
- A gateway now refuses to start when Docker would resolve the apps' names
  outside the tunnel: keep `dns:` on the gateway service (see below).
- `update.on_start` and the `ru-direct` preset are on by default; set
  `MIHOMYAK_RULES_PRESETS=` (empty) to send all traffic through the VPN.

### Added
- `device.seed` / `MIHOMYAK_DEVICE_SEED`: the device (HWID) is derived from a
  secret phrase, the same on any server and after reinstalls; several gateways
  with one seed are one device for the provider.
- `gateway.allow_dns_leak` / `MIHOMYAK_ALLOW_DNS_LEAK` to accept the DNS leak
  explicitly.
- Documentation rewritten for users: README (Russian and English) with three
  ways to install and autostart, docs/DOCKER.md with several-project setups
  (per-project gateways with one seed, a shared proxy network, a shared
  gateway), docs/FAQ.md; developer docs in docs/dev/.
- Tests: integration tests for the update pipeline and the mihomo API client
  against fake servers, more unit tests, and `tests/e2e/gateway.sh`: a stand
  with a fake panel, two shadowsocks nodes and a target behind them (traffic
  through a node, fake-ip DNS, explicit proxy, bad and hostile panels, device
  limit, Happ formats, fallback, kill switch, restart from cache, clean stop).

### Changed
- `compose.gateway.yml` turns the kill switch on; `compose.proxy.yml` puts the
  proxy on a shared network `mihomyak` that other compose projects can join;
  both download standalone (no build context needed).
- First start without a cached subscription retries network errors after 5, 10,
  20 s… instead of 1, 2, 4 min. Panel refusals keep the slow pace.
- The image healthcheck polls every 2 s during start-up, so apps waiting for the
  gateway start seconds after it.
- Sources grouped by concern (`cli`, `client`, `config`, `mihomo`, `service`,
  `gateway`, `subscription`, `util`); deployment files in `deploy/`.
- Release archives have stable names (`releases/latest/download/mihomyak-<target>.tar.gz`)
  and contain exactly the binaries CI built and tested; published archives and
  images are verified on amd64 and arm64 after every release.
- CI: images pack the build job's binaries instead of compiling again; the test
  suite also runs for aarch64 and armv7 (qemu); the gateway stand runs natively
  on amd64 and arm64; coverage is reported.

### Fixed
- **Gateway DNS leak on compose networks.** Docker's embedded DNS resolves
  host-inherited upstreams from the host's network namespace, so apps behind the
  gateway got real addresses from the host's resolver instead of mihomo's
  fake-ip. The gateway now has `dns:` in compose and refuses to start without it.
- Apps behind a restarted gateway were left without any network; `depends_on …
  restart: true` makes Compose restart them.
- The Docker image inherited `VOLUME /root/.config/mihomo` from the base image:
  every container left a ~28 MB anonymous volume behind.
- A panel trickling data until the 90 s deadline was reported as "no data for
  30s"; the permission warning showed for config files without secrets; the
  first refused update logged "keeping the current config" when there was none.

## [0.1.0] - 2026-09-26

First version of mihomyak (a rewrite of the mihoro fork).

### Added
- Byte-exact subscription requests of FlClashX 0.4.2, Koala Clash 1.4.1 and
  Happ Desktop 4.3.0 for Linux (User-Agent, HWID formulas, device headers, header
  order and case), verified with golden fixtures captured from the real clients.
- Subscription analysis: provider headers (traffic, expiry, title, interval,
  announce), Remnawave HWID refusals and placeholder stubs that never replace a
  working config.
- Formats: mihomo YAML, share links (plain/base64) via mihomo providers, Xray JSON
  converted to mihomo proxies.
- Supervisor (`mihomyak run`): scheduled updates (interval / cron in local time /
  on start), SIGHUP updates, hot reload, crash restarts with backoff, PID 1 reaping.
- Node filters (include/exclude globs), auto-switching groups
  (fallback/url-test/load-balance) with default selection, rule presets
  (`ru-direct`) and prepended rules.
- Transparent TUN gateway for containers; hardened Docker image and compose files.
- CLI: status, proxies, select, test, mode, fetch, identity, render, check, health,
  core install; ratatui TUI.
- Secure defaults: loopback-only proxy, private-range LAN allowlist, optional proxy
  auth, API secret, umask 077 / 0700 data directory.
- CI (lint, tests incl. real mihomo validation, MSRV, rustdoc, cross builds,
  cargo-deny, docker) and release workflows (static binaries + multi-arch image,
  published only after CI and a tag/version check).
- CLI: `run`, `update`, `status`, `proxies`, `select`, `test`, `mode`, `tui`, `fetch`,
  `identity`, `render`, `check`, `health`, `core install`, `core version`.

### Security (full project review before the first release)
- Subscriptions are untrusted input: only an allowlist of top-level keys is used
  (no listeners, tunnels, ports, controller, TUN, geodata URLs, DNS listen),
  overlay-network proxy types are dropped, provider download paths are fixed and
  local file providers removed.
- Hardened HTTP client: bounded lines/headers/chunks/trailers/decompression,
  wall-clock deadline, 1xx handling, RFC 3986 redirects, no https→http downgrade,
  URL errors never echo the token, CR/LF can never reach request headers.
- Configs are validated with `mihomo -t` before use and rolled back from `*.prev`
  when the running core rejects a reload.
- mihomo runs with a scrubbed environment and exits with the supervisor
  (`PR_SET_PDEATHSIG`); supervisor lock via `flock`; `core install` from a mirror
  requires `--sha256`.
- Provider-controlled text is stripped of control characters before logging or
  printing.

### Added (after the review)
- Opt-in gateway kill switch (`gateway.kill_switch`, `MIHOMYAK_KILL_SWITCH`):
  while mihomo is down only DNS, private networks, replies and the marked traffic
  of mihomo/mihomyak may leave; installed atomically with iptables-restore.
- Internationalised subscription hosts (`.рф`) are converted to punycode.

- Grouped, coloured `--help` with examples; `--version` lists emulated clients.
- CI/CD: static builds for three architectures, Docker e2e test, `edge` image
  from `main`, releases with binaries and a multi-arch image; `Makefile` and
  `scripts/e2e-docker.sh` for local runs.

### Changed
- FlClashX header order is computed with a model of Dart's `HashMap` for any
  header set; os-release is parsed the device_info_plus way (with lsb-release
  fallback). Happ `Accept-Language` follows Qt's rules.
- Gateway mode keeps the subscription panel off the tunnel (fake-ip filter, route
  exclusions), so updates work even when every node is down.
- Cron: Vixie day semantics for `*/n`, case-insensitive aliases, impossible dates
  rejected, UTC offset logged at start. Minimum update interval is 5 minutes
  everywhere.
