#!/usr/bin/env sh
# End-to-end smoke test of a mihomyak image against the fake panel in dev/.
#
#   docker build -t mihomyak:local . && ./scripts/e2e-docker.sh mihomyak:local
#
# Runs two hardened containers (explicit proxy; TUN gateway with kill switch),
# waits until mihomo is healthy and checks that the subscription was applied.
# Needs docker, python3 and /dev/net/tun. Used by CI.
set -eu

image="${1:?usage: $0 <image>}"
port="${MIHOMYAK_E2E_PORT:-18080}"
cd "$(dirname "$0")/.."

# Containers reach the host through the default bridge's gateway address.
host_ip=$(docker network inspect bridge -f '{{(index .IPAM.Config 0).Gateway}}')
ss_link="ss://$(printf 'aes-128-gcm:e2e-password' | base64 | tr -d '\n=')@203.0.113.10:8388"
log_dir=$(mktemp -d)

python3 dev/mock_panel.py --host 0.0.0.0 --port "$port" --device-limit 2 \
  --proxy "$ss_link#NL-1" --proxy "$ss_link#DE-1" >"$log_dir/panel.log" 2>&1 &
panel_pid=$!

cleanup() {
  kill "$panel_pid" 2>/dev/null || true
  docker rm -f mihomyak-e2e-proxy mihomyak-e2e-gateway >/dev/null 2>&1 || true
  rm -rf "$log_dir"
}
trap cleanup EXIT INT TERM

fail() {
  echo "FAIL: $*" >&2
  for name in mihomyak-e2e-proxy mihomyak-e2e-gateway; do
    docker logs "$name" 2>&1 | tail -40 | sed "s/^/[$name] /" >&2 || true
  done
  sed 's/^/[panel] /' "$log_dir/panel.log" >&2
  exit 1
}

# Starts a hardened container and waits until mihomo's API answers.
start() {
  name=$1
  shift
  docker run -d --name "$name" --read-only --tmpfs /tmp --tmpfs /data \
    --cap-drop ALL --security-opt no-new-privileges \
    -e "MIHOMYAK_SUB_URL=http://$host_ip:$port/sub/e2e-$name" "$@" "$image" >/dev/null
  tries=0
  until docker exec "$name" mihomyak health >/dev/null 2>&1; do
    tries=$((tries + 1))
    [ "$tries" -le 60 ] || fail "$name did not become healthy"
    sleep 1
  done
  docker exec "$name" mihomyak status
  docker exec "$name" mihomyak proxies remna | grep -q 'NL-1' || fail "$name: nodes missing"
}

docker run --rm "$image" --version
docker run --rm "$image" --help >/dev/null

echo "== explicit proxy"
start mihomyak-e2e-proxy

echo "== TUN gateway with kill switch"
start mihomyak-e2e-gateway --device /dev/net/tun --cap-add NET_ADMIN \
  -e MIHOMYAK_GATEWAY=1 -e MIHOMYAK_KILL_SWITCH=1
docker exec mihomyak-e2e-gateway iptables -S MIHOMYAK | grep -q REJECT \
  || fail "kill switch chain missing"
docker exec mihomyak-e2e-gateway mihomyak check >/dev/null || fail "mihomyak check"

echo "e2e OK"
