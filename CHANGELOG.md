# Changelog

Format: [Keep a Changelog](https://keepachangelog.com/). Versions follow SemVer.

## [Unreleased]

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
- CI (lint, tests incl. real mihomo validation, cross builds, cargo-deny, docker) and
  release workflows (static binaries + multi-arch image).
