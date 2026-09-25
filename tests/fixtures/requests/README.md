# Golden subscription requests

Raw HTTP request heads captured from the real clients on Ubuntu 24.04 with
`/etc/machine-id = 0d0af05ee8fd4dc29275718f2ce4dff1` (see `docs/SUBSCRIPTIONS.md` §7).
`{PORT}` stands for the test server port. CRLF line endings are significant.

| File | Captured from |
|------|---------------|
| `flclashx-0.4.2-linux.http` | FlClashX v0.4.2 `.deb`, headless under Xvfb, profile auto-update |
| `koala-1.4.1-linux.http` | Koala Clash 1.4.1 `createProfile()` code path replayed with the app's own bundled axios 1.15.1 in its Electron 37 Node runtime; `x-hwid` substituted with the value `deviceInfo.ts` computes for the machine-id above |

| `happ-4.3.0-linux-x64.http` | Happ Desktop 4.3.0 x64 `.deb`, headless under Xvfb via `happ://add/…`, with hostname `rpi-box` and machine-id `11112222333344445555666677778888` (bind-mounted); the UA marker digit (`6`) depends on the day, see `emulation::happ_day_marker` |

`tests/golden_requests.rs` asserts that mihomyak sends these bytes exactly.
Re-capture and update both the fixture and the version constants in
`src/emulation.rs` when a client release changes its request.
