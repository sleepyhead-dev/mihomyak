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
  `url-test`), optionally Russian sites go direct (`ru-direct`).
- **Light and safe.** 2–3 MB of memory (plus ~40 MB for mihomo), a single static binary.
  The subscription is treated as untrusted: the provider cannot open ports on your
  server. The container runs with minimal privileges.
- **Convenient.** `status`, `select`, `test` commands and an interactive `tui` in the
  terminal.

## Installation

You need Linux and a subscription — a link like `https://…/sub/…` from a bot or the
provider's personal dashboard. Supported: `amd64`, `arm64` (Raspberry Pi 3/4/5 with a
64-bit OS), and `armv7`. Docker modes need
[Docker](https://docs.docker.com/engine/install/) installed.

```sh
curl -fsSL https://github.com/sleepyhead-dev/mihomyak/releases/latest/download/install.sh | sh
```

The installer asks for the link, the mode, and a device seed, and takes care of the rest:

- **gateway** (default): containers connected to it reach the internet only through the
  VPN, including DNS;
- **proxy**: HTTP and SOCKS5 on `mihomyak:7890` for containers of any compose project;
- **without Docker**: a systemd service with a proxy on `127.0.0.1:7890`.

It creates `/opt/mihomyak` with the settings (`.env`, mode `600`), starts mihomyak, waits
for it to come up, enables autostart after a reboot, and installs the `mihomyak` command
(`mihomyak status`, `mihomyak tui`, `mihomyak logs`, `mihomyak upgrade`). At the end it
shows how to connect your own container.

The seed is any word or phrase (e.g. `apple`) the device is derived from: the same seed
gives the same device at the provider on any server. Press Enter for a random seed.

Without prompts, e.g. for scripts:

```sh
curl -fsSL https://github.com/sleepyhead-dev/mihomyak/releases/latest/download/install.sh \
  | sh -s -- --yes --url 'https://…/sub/…' --mode gateway --seed apple
```

Update: `mihomyak upgrade`. Remove: `mihomyak uninstall`.

### Connect your container to the gateway

Add a service to `/opt/mihomyak/compose.yml` and run `docker compose up -d` in that
folder:

```yaml
  my-app:
    image: my/app
    network_mode: service:mihomyak        # the whole app's networking goes through the VPN
    depends_on:
      mihomyak: {condition: service_healthy, restart: true}
    restart: unless-stopped
```

The app's ports are published on the `mihomyak` service. How to connect projects from
other folders, and what to pick for several projects — see
[docs/DOCKER.md](docs/DOCKER.md).

<details>
<summary>Manual installation</summary>

Docker gateway (for the proxy, use `compose.proxy.yml`):

```sh
sudo mkdir -p /opt/mihomyak && sudo chown "$USER" /opt/mihomyak && cd /opt/mihomyak
curl -fsSL -o compose.yml https://github.com/sleepyhead-dev/mihomyak/releases/latest/download/compose.gateway.yml
curl -fsSL -o .env https://github.com/sleepyhead-dev/mihomyak/releases/latest/download/env.example
chmod 600 .env && nano .env                 # paste your link into MIHOMYAK_SUB_URL, and a seed
docker compose up -d
docker compose exec mihomyak mihomyak status
```

Without Docker (binary for your architecture: `x86_64`, `aarch64`, `armv7`):

```sh
curl -fsSL https://github.com/sleepyhead-dev/mihomyak/releases/latest/download/mihomyak-aarch64-unknown-linux-musl.tar.gz | tar xz
sudo install -m 755 mihomyak-*/mihomyak /usr/local/bin/
sudo mihomyak core install --dest /usr/local/bin/mihomo
sudo install -D -m 600 mihomyak-*/deploy/config.example.toml /etc/mihomyak/config.toml
sudoedit /etc/mihomyak/config.toml          # paste your link into [subscription] url
sudo cp mihomyak-*/deploy/systemd/mihomyak.service /etc/systemd/system/
sudo systemctl enable --now mihomyak
```

</details>

## Commands

`mihomyak --help` shows all commands by group; `mihomyak <command> --help` gives
details. The installer sets up the `mihomyak` command on the host; without it, in
Docker: `docker exec mihomyak mihomyak <command>`. Three more commands exist only on the
host: `mihomyak logs`, `mihomyak restart` (restart the gateway together with the
containers in its network) and `mihomyak upgrade` (update the image).

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

Settings for a Docker install live in `/opt/mihomyak/.env`; after editing, run
`docker compose up -d` in that folder. Main variables:

| Variable | What it sets |
|------------|------------|
| `MIHOMYAK_SUB_URL` | the subscription link |
| `MIHOMYAK_CLIENT` | which client to impersonate: `flclashx` (default), `koala`, `happ` |
| `MIHOMYAK_PLATFORM` | the client's OS: `linux` (default), for Happ also `windows` or `android` |
| `MIHOMYAK_DEVICE_SEED` | the phrase the device (HWID) is derived from. Same seed — same device on any server |
| `MIHOMYAK_UPDATE_CRON` | update schedule, e.g. `0 5 * * *` (time in `TZ`) |
| `MIHOMYAK_RULES_PRESETS` | `ru-direct`: Russian sites go direct, bypassing the VPN (by default all traffic goes through the VPN) |
| `MIHOMYAK_EXCLUDE` | remove nodes by name: `*Россия*;*Info*` |

Everything else (auto-switching groups, custom rules, any mihomo keys) is set in
`config.toml`: [an example with all the keys](deploy/config.example.toml),
[reference](docs/CONFIG.md).

### Which client to impersonate

| Client | Platform | What the panel returns | When to choose it |
|--------|----------|------------------------|-------------------|
| `flclashx` | Linux | mihomo YAML in every panel | if the provider accepts Linux |
| `koala` | Linux | YAML or links | if the provider only allows Koala |
| `happ` | Linux, **Windows**, **Android** | links or Xray JSON (converted) | many providers refuse Linux but accept Happ on Windows or Android |

Set the platform with `MIHOMYAK_PLATFORM=windows` or `android`. The Happ for Windows and
Android requests were captured from the real apps and are reproduced byte for byte.
`mihomyak fetch --client happ --platform windows` shows what the panel would answer
without applying anything.

Each client and platform has its own HWID formula, so switching them means a new device
on the provider's side. Choose once.

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
