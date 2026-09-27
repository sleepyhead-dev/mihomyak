# Changelog

Format: [Keep a Changelog](https://keepachangelog.com/). Versions follow SemVer.

## [Unreleased]

### Fixed
- Gateway DNS leak on compose/user-defined networks: Docker's embedded DNS
  forwards host-inherited upstreams from the host's network namespace, so apps
  behind the gateway got real addresses from the host's resolver instead of
  mihomo's fake-ip. `compose.gateway.yml` now sets `dns:` on the gateway, and
  mihomyak warns at start and in `check` when Docker forwards past the tunnel.
  Found on a Raspberry Pi 3B+ (Docker 29.8); covered by the Docker e2e test.
- Apps behind a restarted gateway (`docker compose restart mihomyak`) were left
  in the old network namespace without any network. The example app in
  `compose.gateway.yml` now has `depends_on: … restart: true`, so Compose
  restarts it after the gateway; documented in docs/DOCKER.md.

### Changed
- CI: images pack the static binaries of the build job instead of compiling
  again in Docker (the multi-arch `edge` image no longer builds three targets);
  the test suite also runs for aarch64 and armv7 under qemu; releases publish
  the binaries CI built and tested; the arm64 image of every PR is an artifact.
  `docker build .` from source still works and is checked on `main`.

### Added
- `dev/lab/`: a hands-on gateway lab for an ARM box (test clients behind the
  gateway, a control container, mock panel and nodes).

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
