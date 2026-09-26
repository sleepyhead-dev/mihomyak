# AGENTS.md

Handoff notes for anyone (human or AI agent) continuing work on mihomyak.
`CLAUDE.md` is a symlink to this file.

## What this is

mihomyak is a lightweight Rust supervisor for [mihomo](https://github.com/MetaCubeX/mihomo)
that fetches CIS-style VPN subscriptions (Remnawave, Marzban, PasarGuard, 3x-ui) while
impersonating FlClashX / Koala Clash / Happ byte for byte, keeps mihomo running, and
serves as a Docker gateway for other containers. CLI + TUI, no web UI.

The repo is a fork of `spencerwooo/mihoro`; the code was fully rewritten (git history
before `docs: research how CIS subscription panels…` is mihoro's).

## Owner preferences (keep them)

- Communicate with the owner in **Russian**. User-facing docs (`README.md`, `docs/*`)
  are Russian; code, comments, commit messages and this file are English.
- Priorities: **security** (the host may hold important data) → **low RAM** → comfort.
  No over-engineering, no web panels. Fine-tuning through the config file; only
  frequently used actions become CLI commands.
- Primary target: **ARM (arm64) in Docker**; bare metal must also work.
- Ask the owner when a decision is genuinely theirs (they said so explicitly).
- Rejected ideas: Telegram notifications (logs are enough; the VPN bot already
  reports subscription state), xray-core as a second core.

## Commands

```sh
make check                                   # = fmt check + both clippy runs + tests + rustdoc
make e2e                                     # docker build + scripts/e2e-docker.sh
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --no-default-features -- -D warnings   # without TUI
cargo test                                   # unit + golden requests
MIHOMYAK_TEST_MIHOMO=/path/to/mihomo cargo test   # + real `mihomo -t` validation
cargo +1.88 test                             # MSRV
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
cargo deny check                             # supply chain (cargo install cargo-deny)
./scripts/build-static.sh aarch64-unknown-linux-musl   # static ARM build (clang + rust-lld)
docker buildx build --platform linux/arm64 -t mihomyak .
python3 dev/mock_panel.py --port 8080 --device-limit 1   # fake Remnawave for e2e
./scripts/e2e-docker.sh mihomyak:local       # hardened containers vs the mock panel
```

CI (`.github/workflows/ci.yml`, on PRs and `main`) runs all of the above, including
the Docker e2e script, uploads static binaries as artifacts and, on `main`,
publishes `ghcr.io/sleepyhead-dev/mihomyak:edge`. `release.yml` (tags `vX.Y.Z`)
reuses CI, checks the tag against `Cargo.toml`, and publishes a GitHub release
(tarballs, SHA256SUMS, notes from the CHANGELOG section) and the `X.Y.Z`/`latest`
image. To release: move the `[Unreleased]` notes under `## [X.Y.Z] - date`, bump
`Cargo.toml`, commit, `git tag vX.Y.Z && git push --tags`.

The CLI help is assembled in `src/cli.rs::command()`: new subcommands must be added
to `GROUPS` (a test enforces it).

## Layout

See `docs/ARCHITECTURE.md` for the module map and design decisions. Quick pointers:

- Client emulation: `src/emulation.rs` (+ golden fixtures in `tests/fixtures/requests/`).
- Subscription understanding: `src/subscription/` (headers, body formats, stubs, Xray JSON).
- Config generation: `src/profile.rs`. Settings schema: `src/config.rs`.
- Supervisor loop: `src/supervisor.rs`; update pipeline: `src/updater.rs`.
- Research on how panels and clients behave: `docs/SUBSCRIPTIONS.md` — read it before
  touching emulation or stub detection.

## Conventions

- Rust 2024, MSRV 1.88, `rustfmt.toml` in repo, default clippy lints, `-D warnings` in CI.
- No new heavy dependencies without a reason (no tokio/reqwest/regex/chrono). The
  binary is ~3 MB static; the supervisor idles at ~2–4 MB RSS.
- Every emulation constant must be backed by source code or a captured request, and
  documented in `docs/SUBSCRIPTIONS.md`. Unverified behaviour is marked as such.
- Secrets never go to logs: use `subscription::redact` for URLs; `http::Url`
  errors never contain the input.
- Everything from the panel is untrusted: new subscription keys go through the
  allowlist in `profile.rs` (`PROVIDER_KEYS`, `PROVIDER_DNS_KEYS`, `PROXY_TYPES`),
  provider text goes through `util::sanitize` before it is printed or logged, and
  the HTTP client bounds every read. Keep it that way.
- A config reaches mihomo only via `Updater` (build → `mihomo -t` → backup →
  write); never write `config.yaml` elsewhere.
- Tests live next to the code; cross-module behaviour goes to `tests/`.
- Conventional commit messages (`feat:`, `fix:`, `docs:`, `build:`…).

## Updating emulated client versions

When FlClashX/Koala/Happ release a new version:

1. Check the source (FlClashX `lib/common/package.dart`, `lib/utils/device_info_service.dart`;
   Koala `src/main/utils/userAgent.ts`, `deviceInfo.ts`, `config/profile.ts`) or, for Happ,
   run the new Linux build headless and capture a request (method: `docs/SUBSCRIPTIONS.md` §7).
2. Update constants in `src/emulation.rs`, the fixture in `tests/fixtures/requests/`, and §7
   of `docs/SUBSCRIPTIONS.md`. Users can override versions without a rebuild via
   `subscription.app_version` / `app_build` / `core_version`.
3. Happ's UA contains a build id and a daily marker; re-check both with the disassembly
   method in §7.3 if the format changes.

## Environment notes (from the original cloud sandbox)

- GitHub web/API and `dl-cdn.alpinelinux.org` were blocked; release downloads, raw
  GitHub and git clones worked. Docker builds of the Alpine build stage could not run
  there, so the runtime image was verified with a host-built static binary instead
  (`FROM metacubex/mihomo` + `COPY mihomyak`).
- Pushing works; early in the project it failed with 403 until GitHub access was
  fixed.

## Review history

Before the first release the whole project was reviewed by four independent
agents (code correctness, security, runtime/e2e, structure/docs) and every
finding was fixed or documented. The main results are listed under "Security" in
`CHANGELOG.md`. Deliberately left as documented limitations: TLS fingerprint
differences and no `happ://crypt` support. The gateway fail-open was later
addressed with the opt-in kill switch (`src/killswitch.rs`), IDN hosts with a
built-in punycode encoder (`http::to_ascii_domain`).

## Status and ideas

Done and verified end to end: see `CHANGELOG.md`.

Possible next steps (discuss with the owner first):

- Several subscriptions merged into one config (was considered, not requested yet).
- Xray JSON: `sockopt.dialerProxy` chains / fragment, kcp and hysteria `finalmask`
  are not converted (logged). Revisit if providers depend on them.
- TLS fingerprint (JA3/JA4) differs from the real clients (rustls); only matters if
  a panel sits behind fingerprinting anti-bot protection.
- LAN gateway scenario (`network_mode: host` + ip_forward) is documented but was not
  tested end to end.
- Kill switch covers traffic leaving the namespace (`OUTPUT`), not traffic the
  host forwards for LAN clients (`FORWARD`, scenario 3). Extend only if needed.
- A TLS ClientHello closer to the real clients is possible (see the discussion in
  `docs/SUBSCRIPTIONS.md` §8) but costs a C/C++ TLS stack; not planned.
