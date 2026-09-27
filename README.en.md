<div align="center">

# mihomyak

**VPN subscription for your server: proxy and transparent gateway for Docker containers**

[![CI](https://github.com/sleepyhead-dev/mihomyak/actions/workflows/ci.yml/badge.svg)](https://github.com/sleepyhead-dev/mihomyak/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/sleepyhead-dev/mihomyak)](https://github.com/sleepyhead-dev/mihomyak/releases)
[![Platforms](https://img.shields.io/badge/platforms-amd64%20%7C%20arm64%20%7C%20armv7-blue)](#installation)
[![License: MIT](https://img.shields.io/badge/license-MIT-green)](LICENSE)

[Русский](README.md) · English

</div>

mihomyak takes your subscription from the provider's panel (Remnawave, Marzban, PasarGuard,
3x-ui), presents itself to it as a real client (FlClashX, Koala Clash, or Happ), and keeps
the [mihomo](https://github.com/MetaCubeX/mihomo) core running. The result is a VPN for a
home server, Raspberry Pi, or VPS: an HTTP/SOCKS proxy for programs and a transparent
gateway for Docker containers that don't need to know anything about the proxy.

```
provider panel ──subscription──▶ mihomyak ──verified config──▶ mihomo ──▶ VPN nodes ──▶ internet
                                                                   ▲
        your containers (network_mode: service:mihomyak) ──────────┘  all traffic and DNS
```

## What it does

- **Looks like a real client.** The subscription request byte-for-byte matches FlClashX,
  Koala Clash, or Happ: User-Agent, HWID, device headers, their order and case. The
  device is derived from a seed phrase and survives a reinstall.
- **Doesn't break a working VPN.** Stub responses like "Device limit", "Subscription
  expired", panel errors, and configs that fail `mihomo -t` validation do not replace a
  working config. After a restart, the network comes up immediately from the cache.
- **Gateway for containers.** Any container in the gateway's network goes through the
  VPN, including DNS. The kill switch blocks traffic while the VPN isn't working, instead
  of letting it through directly.
- **Updates itself.** On the provider's interval, on a cron schedule, and at startup.
- **Nodes to your taste.** Filters by name, auto-switching groups (`fallback`,
  `url-test`), Russian sites go direct (`ru-direct`, can be turned off).
- **Light and safe.** 2–3 MB of memory (plus ~40 MB for mihomo), a single static binary.
  The subscription is treated as untrusted: the provider cannot open ports on your
  server. The container runs with minimal privileges.
- **Convenient.** `status`, `select`, `test` commands and an interactive `tui` in the
  terminal.

## Installation

You need Linux and a subscription — a link like `https://…/sub/…` from a bot or the
provider's personal dashboard. Prebuilt images and binaries are available for `amd64`,
`arm64` (Raspberry Pi 3/4/5 with a 64-bit OS), and `armv7`.

### Option 1. Docker gateway (recommended)

Containers connected to the gateway reach the internet only through the VPN.

```sh
mkdir -p ~/mihomyak && cd ~/mihomyak
curl -fsSLO https://raw.githubusercontent.com/sleepyhead-dev/mihomyak/main/deploy/docker/compose.gateway.yml
curl -fsSL -o .env https://raw.githubusercontent.com/sleepyhead-dev/mihomyak/main/deploy/docker/.env.example
chmod 600 .env
nano .env                                   # paste your link into MIHOMYAK_SUB_URL
docker compose -f compose.gateway.yml up -d
docker compose -f compose.gateway.yml exec mihomyak mihomyak status
```

The gateway starts itself after a reboot (`restart: unless-stopped`), provided Docker's
autostart is enabled (`sudo systemctl enable docker`, usually already the case).

Your own container connects with a single line in the same `compose.gateway.yml`:

```yaml
  my-app:
    image: my/app
    network_mode: service:mihomyak        # the whole app's networking goes through the VPN
    depends_on:
      mihomyak: {condition: service_healthy, restart: true}
```

The app's ports are published on the `mihomyak` service. How to connect several
projects, update the gateway, and what to do about DNS — see
[docs/DOCKER.md](docs/DOCKER.md).

### Option 2. Proxy for containers and apps

If your app is fine with just `HTTP_PROXY`, use
[`compose.proxy.yml`](deploy/docker/compose.proxy.yml): mihomyak listens on
`http://mihomyak:7890` (HTTP and SOCKS5) on a shared Docker network, and apps point to it
via `HTTP_PROXY`/`ALL_PROXY`. Restarting the proxy doesn't break the app.

### Option 3. Without Docker (systemd)

```sh
# binary for your architecture: x86_64, aarch64, or armv7
curl -fsSL https://github.com/sleepyhead-dev/mihomyak/releases/latest/download/mihomyak-aarch64-unknown-linux-musl.tar.gz | tar xz
sudo install -m 755 mihomyak-*/mihomyak /usr/local/bin/
sudo mihomyak core install --dest /usr/local/bin/mihomo
sudo install -D -m 600 mihomyak-*/deploy/config.example.toml /etc/mihomyak/config.toml
sudoedit /etc/mihomyak/config.toml          # paste your link into [subscription] url
sudo cp mihomyak-*/deploy/systemd/mihomyak.service /etc/systemd/system/
sudo systemctl enable --now mihomyak        # start now and on every reboot
mihomyak status
```

The proxy will be on `127.0.0.1:7890`. To proxy the whole host, enable
`[gateway] enable = true` ([docs/CONFIG.md](docs/CONFIG.md#gateway--прозрачный-шлюз-tun)).

## Commands

`mihomyak --help` shows all commands by group; `mihomyak <command> --help` gives
details. In Docker: `docker exec mihomyak mihomyak <command>`.

| Command | What it does |
|---------|------------|
| `status` | subscription, traffic, expiry date, next update, selected nodes |
| `tui` | interactive screen: nodes, latency, arrow-key selection (`docker exec -it …`) |
| `proxies [group]` | groups, or a group's nodes with latency |
| `select <group> <node>` | select a node (part of the name is enough: `select proxy nl`) |
| `test [group]` | measure latency |
| `mode [rule\|global\|direct]` | routing mode |
| `update` | update the subscription now (exit code 1 if the panel refused) |
| `fetch` | diagnostics: what was sent to the panel, what it answered, and why |
| `identity` | which device mihomyak presents itself as, its HWID |
| `check` / `render` | validate settings / show the resulting mihomo config |
| `run` | the service itself (the container and systemd entry point) |

## Configuration

For Docker, a `.env` next to the compose file is enough. Main variables:

| Variable | What it sets |
|------------|------------|
| `MIHOMYAK_SUB_URL` | the subscription link |
| `MIHOMYAK_CLIENT` | which client to impersonate: `flclashx` (default), `koala`, `happ` |
| `MIHOMYAK_DEVICE_SEED` | the phrase the device (HWID) is derived from. Same seed — same device on any server |
| `MIHOMYAK_UPDATE_CRON` | update schedule, e.g. `0 5 * * *` (time in `TZ`) |
| `MIHOMYAK_RULES_PRESETS` | `ru-direct` by default; an empty value means all traffic goes through the VPN |
| `MIHOMYAK_EXCLUDE` | remove nodes by name: `*Россия*;*Info*` |

Everything else (auto-switching groups, custom rules, any mihomo keys) is set in
`config.toml`: [an example with all the keys](deploy/config.example.toml),
[reference](docs/CONFIG.md).

### Which client to impersonate

| Client | What the panel returns | When to choose it |
|--------|-------------------|----------------|
| `flclashx` | mihomo YAML in every panel | almost always |
| `koala` | YAML or links | if the provider only allows Koala |
| `happ` | links or Xray JSON (converted) | if the provider only allows Happ |

Each client has its own HWID formula, so switching clients means a new device on the
provider's side. Choose a client once.

## If something's wrong

- `mihomyak status` and `docker logs mihomyak` are the first things to check.
- `mihomyak fetch` shows the request sent to the panel, its response, and the verdict.
- Frequently asked questions (device limit, "platform not supported", DNS, containers
  losing networking after the gateway restarts) — see [docs/FAQ.md](docs/FAQ.md).

Known limitations: the request's TLS fingerprint differs from real clients (matters only
if the panel sits behind anti-bot protection); `happ://crypt…` links are not supported.

## Documentation

- [docs/DOCKER.md](docs/DOCKER.md) — Docker: gateway, proxy, multiple projects, updating;
- [docs/CONFIG.md](docs/CONFIG.md) — all settings and environment variables;
- [docs/FAQ.md](docs/FAQ.md) — frequently asked questions and issues;
- [docs/dev/](docs/dev/DEVELOPMENT.md) — for developers: building, tests, code layout,
  panel and client research.

The detailed docs above are in Russian.

## License

[MIT](LICENSE). The project grew out of a fork of
[mihoro](https://github.com/spencerwooo/mihoro) and was completely rewritten.
</content>
</invoke>
