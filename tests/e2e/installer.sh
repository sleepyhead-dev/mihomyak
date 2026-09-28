#!/usr/bin/env bash
# Installs mihomyak with deploy/install.sh the way the README tells users to,
# non-interactively, against the mock panel: Docker gateway, Docker proxy, then the
# systemd service. Checks the result, update and uninstall. Needs Docker, Compose,
# /dev/net/tun, systemd and passwordless sudo (a CI runner); leaves nothing behind.
#
#   ./tests/e2e/installer.sh mihomyak:local target/x86_64-unknown-linux-musl/release/mihomyak
set -euo pipefail

image="${1:?usage: $0 <image> <static binary>}"
binary=$(realpath "${2:?usage: $0 <image> <static binary>}")
cd "$(dirname "$0")/../.."

port="${MIHOMYAK_E2E_PORT:-18090}"
dir=/opt/mihomyak-e2e
host_ip=$(docker network inspect bridge -f '{{(index .IPAM.Config 0).Gateway}}')
ss_link="ss://$(printf 'aes-128-gcm:e2e-password' | base64 | tr -d '\n=')@203.0.113.10:8388"
log=$(mktemp)

python3 tests/e2e/mock_panel.py --host 0.0.0.0 --port "$port" --device-limit 0 \
  --proxy "$ss_link#NL-1" >"$log" 2>&1 &
panel=$!

cleanup() {
  sh deploy/install.sh uninstall --purge --yes >/dev/null 2>&1 || true
  sudo sh deploy/install.sh uninstall --purge --yes >/dev/null 2>&1 || true
  kill "$panel" 2>/dev/null || true
  rm -f "$log"
}
trap cleanup EXIT

step() { printf '\n== %s\n' "$*"; }
ok() { printf '   ok  %s\n' "$*"; }
fail() {
  printf '\nFAIL: %s\n' "$*" >&2
  docker logs --tail 40 mihomyak 2>&1 | sed 's/^/[mihomyak] /' >&2 || true
  sudo journalctl -u mihomyak -n 40 --no-pager 2>&1 | sed 's/^/[systemd] /' >&2 || true
  sed 's/^/[panel] /' "$log" >&2
  exit 1
}

for mode in gateway proxy; do
  step "Docker $mode"
  # The proxy run also installs Happ for Windows, the typical choice when a
  # provider refuses Linux.
  extra=()
  [[ $mode == proxy ]] && extra=(--client happ --platform windows)
  sh deploy/install.sh --yes --mode "$mode" --url "http://$host_ip:$port/sub/$mode" \
    --from deploy/docker --image "$image" --dir "$dir" "${extra[@]}" || fail "install ($mode)"
  [[ $(stat -c %a "$dir/.env") == 600 ]] || fail ".env is not private"
  grep -Eq '^MIHOMYAK_DEVICE_SEED=[0-9a-f]{24}$' "$dir/.env" || fail "no generated seed"
  status=$(mihomyak status) || fail "the mihomyak command on the host"
  grep -q 'Mock VPN' <<<"$status" || fail "status: $status"
  ok "installed, healthy, the host command works"
  if [[ $mode == proxy ]]; then
    identity=$(mihomyak identity) || fail "identity"
    grep -q 'client:        happ on windows' <<<"$identity" || fail "identity: $identity"
    grep -q 'User-Agent: Happ/4.3.0/Windows/' <<<"$identity" || fail "identity: $identity"
    ok "Happ for Windows"
  fi
  sh deploy/install.sh update --yes >/dev/null || fail "update ($mode)"
  ok "update"
  sh deploy/install.sh uninstall --purge --yes >/dev/null || fail "uninstall ($mode)"
  if docker inspect mihomyak >/dev/null 2>&1; then fail "container left behind"; fi
  if docker volume inspect mihomyak_data >/dev/null 2>&1; then fail "volume left behind"; fi
  [[ ! -e $dir && ! -e /usr/local/bin/mihomyak ]] || fail "files left behind"
  ok "uninstall removes the container, the volume, $dir and the command"
done

step "systemd"
sudo sh deploy/install.sh --yes --mode systemd --url "http://127.0.0.1:$port/sub/systemd" \
  --seed apple --binary "$binary" || fail "install (systemd)"
systemctl is-enabled --quiet mihomyak || fail "the service is not enabled at boot"
[[ $(sudo stat -c %a /etc/mihomyak/config.toml) == 600 ]] || fail "config.toml is not private"
status=$(sudo mihomyak status) || fail "status (systemd)"
grep -q 'Mock VPN' <<<"$status" || fail "status: $status"
code=$(curl -s -m 10 -x http://127.0.0.1:7890 -o /dev/null -w '%{http_code}' http://example.com || true)
[[ $code != 000 ]] || fail "the proxy port does not answer"
ok "installed, enabled at boot, healthy, proxy listening on 127.0.0.1:7890"
sudo sh deploy/install.sh uninstall --purge --yes >/dev/null || fail "uninstall (systemd)"
if systemctl cat mihomyak >/dev/null 2>&1; then fail "unit left behind"; fi
[[ ! -e /etc/mihomyak && ! -e /usr/local/bin/mihomyak ]] || fail "files left behind"
ok "uninstall removes the service and its files"

printf '\ninstaller e2e OK\n'
