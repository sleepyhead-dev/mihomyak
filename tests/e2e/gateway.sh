#!/usr/bin/env bash
# End-to-end test of the TUN gateway on the stand in gateway.compose.yml:
# traffic through a node, fake-ip DNS, kill switch, fallback, restarts, bad and
# hostile panels, device limit, Happ formats, explicit proxy. Needs Docker with
# Compose and /dev/net/tun; creates only mhk-* resources and removes them.
#
#   ./tests/e2e/gateway.sh mihomyak:local
set -euo pipefail

image="${1:?usage: $0 <image>}"
cd "$(dirname "$0")"
export MIHOMYAK_IMAGE="$image"

compose() { docker compose -f gateway.compose.yml "$@"; }
gw() { docker exec mhk-gw "$@"; }
app() { docker exec mhk-app "$@"; }

step() { printf '\n== %s\n' "$*"; }
ok() { printf '   ok  %s\n' "$*"; }
fail() {
  printf '\nFAIL: %s\n' "$*" >&2
  compose ps -a >&2 || true
  for c in mhk-gw mhk-proxy mhk-panel; do
    docker logs --tail 50 "$c" 2>&1 | sed "s/^/[$c] /" >&2 || true
  done
  exit 1
}
cleanup() {
  docker rm -f mhk-e2e-stop >/dev/null 2>&1 || true
  compose down -v --timeout 5 >/dev/null 2>&1 || true
}
trap cleanup EXIT

# Retries a command for up to $1 seconds.
wait_for() {
  local seconds=$1 what=$2
  shift 2
  for _ in $(seq "$seconds"); do
    "$@" >/dev/null 2>&1 && return 0
    sleep 1
  done
  fail "timed out waiting for: $what"
}

# The target answers with the address it was reached from: a node's 203.0.113.2x.
target() { app curl -sS -m 10 http://203.0.113.80/ 2>&1 || true; }
expect_node() {
  local seen
  seen=$(target)
  [[ $seen == ${1:-203.0.113.2[12]} ]] || fail "$2: the target saw '$seen'"
  ok "$2 ($seen)"
}
target_is() { [[ $(target) == "$1" ]]; }
panel_state() { app curl -sS -m 5 -X POST "http://10.203.0.10:8080/_control?state=$1" >/dev/null; }
fetch_as() { docker exec "$@" mhk-gw mihomyak fetch 2>&1 || true; }

step "stand"
cleanup
compose up -d --quiet-pull
wait_for 90 "gateway healthy" gw mihomyak health
wait_for 90 "proxy healthy" docker exec mhk-proxy mihomyak health
ok "gateway and proxy are healthy"

step "hardening"
inspect=$(docker inspect -f '{{.HostConfig.ReadonlyRootfs}} {{.HostConfig.CapDrop}} {{.HostConfig.CapAdd}} {{.HostConfig.SecurityOpt}}' mhk-gw)
[[ $inspect == "true [ALL] ["*"NET_ADMIN] [no-new-privileges:true]" ]] || fail "gateway runs with: $inspect"
ok "read-only, only NET_ADMIN, no-new-privileges"

step "traffic"
expect_node "" "a client behind the gateway reaches the target through a node"
addr=$(docker run --rm --network container:mhk-gw alpine:3.22 nslookup -type=a example.com 2>&1 \
  | awk '/^Address/ { last = $2 } END { print last }')
[[ $addr == 198.18.* ]] || fail "app DNS bypassed mihomo: example.com -> '$addr'"
ok "app DNS is answered by mihomo with a fake-ip ($addr)"
proxied=$(docker exec mhk-client curl -sS -m 10 -x http://10.203.0.3:7890 http://203.0.113.80/ 2>&1 || true)
[[ $proxied == 203.0.113.2[12] ]] || fail "explicit proxy: the target saw '$proxied'"
ok "explicit proxy for a neighbour container ($proxied)"

step "CLI"
gw mihomyak status | grep -q 'mihomo:       v' || fail "status does not show mihomo"
gw mihomyak proxies E2E-FALLBACK | grep -q 'E2E-2' || fail "proxies misses a node"
ok "status, proxies"
started=$(date +%s%N)
gw mihomyak update >/dev/null || fail "update failed"
elapsed=$((($(date +%s%N) - started) / 1000000))
((elapsed < 5000)) || fail "update took ${elapsed} ms"
ok "update through the supervisor (SIGHUP) in ${elapsed} ms"

step "a bad panel never replaces the working config"
for state in http500 expired broken; do
  panel_state "$state"
  if out=$(gw mihomyak update 2>&1); then
    fail "panel state $state was applied: $out"
  fi
  expect_node "" "panel answers '$state': not applied, traffic still flows"
done
panel_state good
gw mihomyak update >/dev/null || fail "a good answer after errors was not applied"
ok "a good answer applies again"

step "device limit (3 devices: gateway, proxy, one more)"
out=$(fetch_as -e MIHOMYAK_MACHINE_ID=00000000000000000000000000000001)
[[ $out == *"OK, usable"* ]] || fail "third device refused: $out"
out=$(fetch_as -e MIHOMYAK_MACHINE_ID=00000000000000000000000000000002)
[[ $out == *"max-devices-reached=true"* && $out == *REJECTED* ]] || fail "fourth device: $out"
ok "the fourth device is refused as a device-limit stub"

step "Happ formats"
out=$(fetch_as -e MIHOMYAK_CLIENT=happ -e MIHOMYAK_SUB_URL=http://10.203.0.11:8080/sub/happ)
[[ $out == *"xray-json (1 proxies)"* ]] || fail "Xray JSON: $out"
ok "Xray JSON"
out=$(fetch_as -e MIHOMYAK_CLIENT=happ -e MIHOMYAK_SUB_URL=http://10.203.0.12:8080/sub/happ)
[[ $out == *"links (1 proxies)"* ]] || fail "base64 links: $out"
ok "base64 links"

step "hostile panel"
for mode in hang huge bighead manyheaders badchunk hugechunk gzipbomb loop garbage; do
  started=$SECONDS
  if docker exec -e "MIHOMYAK_SUB_URL=http://10.203.0.13:8081/sub/$mode" mhk-gw \
    timeout 120 mihomyak fetch >/dev/null 2>&1; then
    fail "hostile panel mode $mode was accepted"
  fi
  ((SECONDS - started < 100)) || fail "hostile panel mode $mode took $((SECONDS - started)) s"
done
gw mihomyak health >/dev/null || fail "mihomo is down after the hostile panel"
ok "every mode fails in time, the gateway keeps running"

step "fallback"
wait_for 30 "E2E-1 preferred" target_is 203.0.113.21
compose stop -t 1 mhk-node1 >/dev/null 2>&1
wait_for 60 "switch to E2E-2" target_is 203.0.113.22
ok "node 1 down: traffic moves to node 2"
compose start mhk-node1 >/dev/null 2>&1
wait_for 60 "return to E2E-1" target_is 203.0.113.21
ok "node 1 back: traffic returns"

step "kill switch"
gw kill -STOP 1
gw sh -c 'kill -9 "$(pidof mihomo)"'
sleep 1
out=$(app curl -sS -m 5 -o /dev/null https://1.1.1.1 2>&1 || true)
[[ $out == *"Could not connect"* || $out == *refused* ]] || fail "traffic while mihomo is down: '$out'"
ok "mihomo dead, supervisor frozen: outgoing traffic is refused"
gw mihomyak fetch >/dev/null || fail "the supervisor's own requests are blocked"
ok "mihomyak still reaches the panel"
gw kill -CONT 1
wait_for 30 "mihomo restarted" gw mihomyak health
expect_node "" "after the restart traffic goes through a node again"

step "restart"
compose restart mhk-gw >/dev/null 2>&1
wait_for 60 "gateway healthy" gw mihomyak health
wait_for 60 "client reconnected" target_is 203.0.113.21
docker logs mhk-gw 2>&1 | grep -q "using the cached subscription" || fail "no start from cache"
ok "the gateway starts from the cache and its client follows"

step "clean stop"
docker run --rm --name mhk-e2e-stop --network mhk-e2e-wan --dns 1.1.1.1 \
  --read-only --tmpfs /tmp --tmpfs /data --cap-drop ALL --cap-add NET_ADMIN \
  --device /dev/net/tun --security-opt no-new-privileges \
  -e MIHOMYAK_GATEWAY=1 -e MIHOMYAK_KILL_SWITCH=1 \
  -e MIHOMYAK_SUB_URL=http://10.203.0.11:8080/sub/stop \
  --entrypoint sh "$image" -c '
    mihomyak run 2>/dev/null &
    until mihomyak health >/dev/null 2>&1; do sleep 1; done
    iptables -S OUTPUT | grep -q MIHOMYAK || { echo "no kill switch while running"; exit 1; }
    kill -TERM $! && wait $!
    if iptables -S MIHOMYAK >/dev/null 2>&1; then echo "chain left behind"; exit 1; fi
    if ip rule | grep -q 2022; then echo "TUN routes left behind"; exit 1; fi' \
  || fail "SIGTERM did not clean up"
ok "SIGTERM removes the kill switch chain and the TUN routes"

step "resources"
rss=$(gw awk '/^VmRSS/ { print $2 }' /proc/1/status)
((rss < 16384)) || fail "supervisor RSS is ${rss} KiB"
ok "supervisor RSS ${rss} KiB"

printf '\ngateway e2e OK\n'
