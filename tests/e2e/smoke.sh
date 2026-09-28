#!/usr/bin/env sh
# End-to-end smoke test of a mihomyak image against the fake panel in tests/e2e/.
#
#   docker build -t mihomyak:local . && ./tests/e2e/smoke.sh mihomyak:local
#
# Checks --version/--help, the image's declared volumes, a hardened explicit-proxy
# container becoming healthy with nodes and seeded geodata, and that a gateway on
# a user-defined network without --dns refuses to start rather than leak (the TUN
# gateway, kill switch and app DNS are covered more thoroughly by gateway.sh).
# Needs docker, python3 and /dev/net/tun. Used by CI.
set -eu

image="${1:?usage: $0 <image>}"
port="${MIHOMYAK_E2E_PORT:-18080}"
cd "$(dirname "$0")/../.."

# Containers reach the host through the default bridge's gateway address.
host_ip=$(docker network inspect bridge -f '{{(index .IPAM.Config 0).Gateway}}')
ss_link="ss://$(printf 'aes-128-gcm:e2e-password' | base64 | tr -d '\n=')@203.0.113.10:8388"
log_dir=$(mktemp -d)

python3 tests/e2e/mock_panel.py --host 0.0.0.0 --port "$port" --device-limit 4\
  --proxy "$ss_link#NL-1" --proxy "$ss_link#DE-1" >"$log_dir/panel.log" 2>&1 &
panel_pid=$!

containers="mihomyak-e2e-proxy mihomyak-e2e-nodns"

cleanup() {
  kill "$panel_pid" 2>/dev/null || true
  # shellcheck disable=SC2086
  docker rm -f $containers >/dev/null 2>&1 || true
  docker network rm mihomyak-e2e >/dev/null 2>&1 || true
  rm -rf "$log_dir"
}
trap cleanup EXIT INT TERM

fail() {
  echo "FAIL: $*" >&2
  for name in $containers; do
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
# Only /data: an inherited volume would leave an anonymous one per container.
volumes=$(docker image inspect -f '{{json .Config.Volumes}}' "$image")
[ "$volumes" = '{"/data":{}}' ] || fail "unexpected image volumes: $volumes"

echo "== explicit proxy"
start mihomyak-e2e-proxy
docker exec mihomyak-e2e-proxy test -s /data/mihomo/geoip.metadb || fail "geodata not seeded"

echo "== gateway without --dns refuses to start"
# On a user-defined network Docker's embedded DNS forwards host-inherited
# upstreams from the host's namespace, past the TUN; an explicit --dns is
# queried from the gateway's namespace and answered by mihomo (src/gateway/mod.rs).
# Without --dns the gateway refuses to start rather than leak.
docker network create mihomyak-e2e >/dev/null
if nodns=$(docker run --rm --name mihomyak-e2e-nodns --network mihomyak-e2e \
  --read-only --tmpfs /data --cap-drop ALL --cap-add NET_ADMIN --device /dev/net/tun \
  -e MIHOMYAK_GATEWAY=1 -e "MIHOMYAK_SUB_URL=http://$host_ip:$port/sub/e2e-nodns" \
  "$image" 2>&1); then
  fail "a leaking gateway started"
fi
echo "$nodns" | grep -q "host's resolver" || fail "no DNS leak message: $nodns"

echo "e2e OK"
