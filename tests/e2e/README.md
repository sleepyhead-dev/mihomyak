# End-to-end tests

Both scripts take an image and create only `mhk-*` / `mihomyak-e2e-*` resources,
removed on exit. They need Docker with Compose and `/dev/net/tun`.

```sh
docker build -t mihomyak:local .
./tests/e2e/smoke.sh mihomyak:local     # hardened containers, DNS leak refusal
./tests/e2e/gateway.sh mihomyak:local   # the full gateway stand
```

| File | Role |
|------|------|
| `smoke.sh` | explicit proxy and TUN gateway against `mock_panel.py` on the host |
| `gateway.sh`, `gateway.compose.yml` | gateway stand: panels, two shadowsocks nodes, a target reachable only through them |
| `gateway.toml` | the stand gateway's config (a fallback group) |
| `mock_panel.py` | Remnawave-like panel: rules by User-Agent, device limit, stubs, Xray JSON, control endpoint |
| `hostile_panel.py` | a panel that misbehaves on purpose (hangs, floods, bombs) |
| `node-ss.yaml`, `target.conf` | mihomo as a shadowsocks server; nginx answering with the caller's address |

What `gateway.sh` covers: see [docs/dev/ARCHITECTURE.md](../../docs/dev/ARCHITECTURE.md#тесты).
