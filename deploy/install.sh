#!/bin/sh
# mihomyak installer: Docker gateway, Docker proxy or a systemd service.
#
#   curl -fsSL https://github.com/sleepyhead-dev/mihomyak/releases/latest/download/install.sh | sh
#   sh install.sh [install] [--url URL] [--mode gateway|proxy|systemd] [--client NAME] [--platform OS]
#                 [--seed WORD] [--dir DIR] [--yes] [--no-cli]
#   sh install.sh update | uninstall [--purge] [--yes]
#
# Without --yes it asks for what is missing (from the terminal, also when piped).
# Testing hooks: --from DIR (compose files of a checkout instead of the release),
# --image NAME (image instead of ghcr.io/…:latest), --binary FILE (systemd mode).
set -eu

REPO=sleepyhead-dev/mihomyak
RELEASE="https://github.com/$REPO/releases/latest/download"
DOCS="https://github.com/$REPO/blob/main/docs"

action=install url= mode= client= platform= seed= dir= yes= cli=1 purge= from= image= binary=
while [ $# -gt 0 ]; do
  case $1 in
    install | update | uninstall) action=$1 ;;
    --url) url=${2:?}; shift ;;
    --mode) mode=${2:?}; shift ;;
    --client) client=${2:?}; shift ;;
    --platform) platform=${2:?}; shift ;;
    --seed) seed=${2:?}; shift ;;
    --dir) dir=${2:?}; shift ;;
    --yes | -y) yes=1 ;;
    --no-cli) cli= ;;
    --purge) purge=1 ;;
    --from) from=${2:?}; shift ;;
    --image) image=${2:?}; shift ;;
    --binary) binary=${2:?}; shift ;;
    -h | --help) sed -n '2,11p' "$0" 2>/dev/null || true; exit 0 ;;
    *) printf 'unknown argument: %s\n' "$1" >&2; exit 2 ;;
  esac
  shift
done

# --- output ------------------------------------------------------------------

case "${LC_ALL:-${LC_MESSAGES:-${LANG:-}}}" in ru*) ru=1 ;; *) ru= ;; esac
# t "по-русски" "in English"
t() { if [ -n "$ru" ]; then printf '%s' "$1"; else printf '%s' "$2"; fi; }
say() { printf '%s\n' "$(t "$1" "$2")"; }
step() { printf '\n\033[1m== %s\033[0m\n' "$(t "$1" "$2")"; }
die() { printf '\033[31m%s\033[0m\n' "$(t "$1" "$2")" >&2; exit 1; }

tty=
if [ -z "$yes" ] && (: </dev/tty) 2>/dev/null; then tty=1; fi
# ask VAR "вопрос" "question" default
ask() {
  var=$1 def=$4
  if [ -z "$tty" ]; then eval "$var=\$def"; return; fi
  printf '%s' "$(t "$2" "$3")" >/dev/tty
  [ -n "$def" ] && printf ' [%s]' "$def" >/dev/tty
  printf ': ' >/dev/tty
  read -r answer </dev/tty || answer=
  eval "$var=\${answer:-\$def}"
}
# confirm "вопрос" "question" y|n  → exit status
confirm() {
  ask reply "$1 (y/n)" "$2 (y/n)" "$3"
  case $reply in [yYдД]*) return 0 ;; *) return 1 ;; esac
}

need() { command -v "$1" >/dev/null 2>&1; }
as_root() { if [ "$(id -u)" = 0 ]; then "$@"; else sudo "$@"; fi; }
can_root() { [ "$(id -u)" = 0 ] || need sudo; }
fetch() { curl -fsSL --retry 3 -o "$2" "$1"; }

# --- Docker ------------------------------------------------------------------

docker_cmd() {
  need docker || die \
    "Docker не установлен. Установите его: https://docs.docker.com/engine/install/ и запустите скрипт снова." \
    "Docker is not installed. Install it: https://docs.docker.com/engine/install/ and run this script again."
  if docker info >/dev/null 2>&1; then
    DOCKER=docker
  elif can_root && as_root docker info >/dev/null 2>&1; then
    DOCKER="as_root docker"
  else
    die "Docker не запущен или нет прав: sudo systemctl enable --now docker" \
        "Docker is not running or not accessible: sudo systemctl enable --now docker"
  fi
  v=$($DOCKER compose version --short 2>/dev/null) || die \
    "Нужен Docker Compose v2 (плагин docker-compose-plugin)." \
    "Docker Compose v2 is required (the docker-compose-plugin package)."
  case $v in
    1.* | 2.[0-9].* | 2.1[0-6].*)
      say "Внимание: Compose $v старше 2.17, контейнеры за шлюзом не будут перезапускаться вместе с ним." \
          "Warning: Compose $v is older than 2.17; containers behind the gateway won't follow its restarts." ;;
  esac
}

# The directory of an existing Docker installation, from the container's labels.
installed_dir() {
  $DOCKER inspect -f '{{index .Config.Labels "com.docker.compose.project.working_dir"}}' mihomyak 2>/dev/null || true
}

pick_dir() {
  [ -n "$dir" ] && return
  existing=$(installed_dir)
  if [ -n "$existing" ]; then dir=$existing; return; fi
  dir=/opt/mihomyak
  if [ ! -w /opt ] && ! can_root; then dir=$HOME/mihomyak; fi
}

make_dir() {
  if mkdir -p "$dir" 2>/dev/null && [ -w "$dir" ]; then return; fi
  as_root mkdir -p "$dir"
  as_root chown "$(id -u):$(id -g)" "$dir"
}

host_tz() {
  if [ -n "${TZ:-}" ]; then printf '%s' "$TZ"; return; fi
  if [ -r /etc/timezone ]; then head -n1 /etc/timezone; return; fi
  link=$(readlink /etc/localtime 2>/dev/null || true)
  case $link in */zoneinfo/*) printf '%s' "${link#*/zoneinfo/}" ;; *) printf UTC ;; esac
}

write_env() {
  umask 077
  {
    printf '# mihomyak settings (install.sh). All variables: %s/CONFIG.md\n' "$DOCS"
    printf '# Apply changes: cd %s && docker compose up -d\n\n' "$dir"
    printf 'MIHOMYAK_SUB_URL=%s\n' "$url"
    printf 'MIHOMYAK_CLIENT=%s\n' "$client"
    [ "$platform" = linux ] || printf 'MIHOMYAK_PLATFORM=%s\n' "$platform"
    printf '# The device the provider sees: the same seed and client, the same device.\n'
    printf 'MIHOMYAK_DEVICE_SEED=%s\n' "$seed"
    printf 'TZ=%s\n' "$(host_tz)"
    [ -n "$image" ] && printf 'MIHOMYAK_IMAGE=%s\n' "$image"
    printf '\n# Russian sites directly, bypassing the VPN:\n# MIHOMYAK_RULES_PRESETS=ru-direct\n'
    printf '# Refresh daily at 05:00 (also at every start):\n# MIHOMYAK_UPDATE_CRON=0 5 * * *\n'
  } >"$dir/.env"
  chmod 600 "$dir/.env"
}

install_cli() {
  [ -n "$cli" ] || return 0
  if [ -z "$yes" ] && ! confirm "Поставить команду mihomyak (/usr/local/bin/mihomyak)?" \
    "Install the mihomyak command (/usr/local/bin/mihomyak)?" y; then return 0; fi
  can_root || return 0
  tmp=$(mktemp)
  cat >"$tmp" <<EOF
#!/bin/sh
# mihomyak on the host: runs its commands in the container (installed by install.sh).
#   mihomyak status | tui | select … | logs | restart | upgrade | uninstall
set -e
if ! docker info >/dev/null 2>&1 && [ "\$(id -u)" != 0 ]; then exec sudo "\$0" "\$@"; fi
case "\${1:-}" in
  logs) shift; exec docker logs -f "\$@" mihomyak ;;
  # Through compose: apps with \`depends_on … restart: true\` follow the gateway
  # (a plain \`docker restart\` would leave them without network).
  restart) cd '$dir' && exec docker compose restart mihomyak ;;
  upgrade) cd '$dir' && docker compose pull && exec docker compose up -d ;;
  uninstall) shift; curl -fsSL $RELEASE/install.sh | exec sh -s -- uninstall "\$@" ;;
esac
if [ -t 0 ] && [ -t 1 ]; then exec docker exec -it mihomyak mihomyak "\$@"; fi
exec docker exec mihomyak mihomyak "\$@"
EOF
  as_root install -m 755 "$tmp" /usr/local/bin/mihomyak
  rm -f "$tmp"
}

wait_healthy() {
  printf '%s' "$(t "Жду запуска" "Waiting for the start")"
  i=0
  while [ $i -lt 90 ]; do
    state=$($DOCKER inspect -f '{{.State.Health.Status}}' mihomyak 2>/dev/null || echo missing)
    [ "$state" = healthy ] && { printf ' ok\n'; return 0; }
    printf .
    sleep 2
    i=$((i + 1))
  done
  printf '\n'
  $DOCKER logs --tail 20 mihomyak 2>&1 || true
  die "mihomyak не запустился. Причина обычно видна выше; подробнее: $DOCS/FAQ.md" \
      "mihomyak did not start. The reason is usually shown above; see $DOCS/FAQ.md"
}

install_docker() {
  docker_cmd
  if [ "$mode" = gateway ] && [ ! -c /dev/net/tun ]; then
    die "Нет /dev/net/tun: шлюзу нужен модуль tun (sudo modprobe tun) или режим proxy." \
        "No /dev/net/tun: the gateway needs the tun module (sudo modprobe tun) or use --mode proxy."
  fi
  pick_dir
  make_dir
  step "Установка в $dir" "Installing into $dir"
  if [ -n "$from" ]; then
    cp "$from/compose.$mode.yml" "$dir/compose.yml"
  else
    fetch "$RELEASE/compose.$mode.yml" "$dir/compose.yml"
  fi
  if [ -f "$dir/.env" ] && [ -z "$url" ]; then
    say "Настройки $dir/.env сохранены." "Keeping the settings in $dir/.env."
  else
    write_env
  fi
  (cd "$dir" && $DOCKER compose pull --quiet 2>/dev/null || true)
  (cd "$dir" && $DOCKER compose up -d)
  wait_healthy
  install_cli
  $DOCKER exec mihomyak mihomyak status || true
  step "Готово" "Done"
  say "Настройки: $dir/.env (ссылка и seed — никому не показывайте)" \
      "Settings: $dir/.env (keep the link and the seed private)"
  say "Команды:   mihomyak status | tui | select | logs | restart | upgrade | uninstall" \
      "Commands:  mihomyak status | tui | select | logs | restart | upgrade | uninstall"
  if [ "$mode" = gateway ]; then
    say "Свой контейнер через VPN — добавьте в $dir/compose.yml и выполните docker compose up -d:" \
        "Route a container through the VPN: add to $dir/compose.yml, then docker compose up -d:"
    cat <<'EOF'

  my-app:
    image: my/app
    network_mode: service:mihomyak
    depends_on:
      mihomyak: {condition: service_healthy, restart: true}
    restart: unless-stopped
EOF
  else
    say "Приложения из любых compose-проектов подключаются так:" \
        "Apps of any compose project connect like this:"
    cat <<'EOF'

  my-app:
    environment:
      HTTP_PROXY: http://mihomyak:7890
      HTTPS_PROXY: http://mihomyak:7890
      ALL_PROXY: socks5h://mihomyak:7890
    networks: [proxy]

networks:
  proxy: {external: true, name: mihomyak}
EOF
  fi
  say "" ""
  say "Несколько проектов, обновление, перенос: $DOCS/DOCKER.md" \
      "Several projects, updates, moving servers: $DOCS/DOCKER.md"
}

# --- systemd -----------------------------------------------------------------

target_triple() {
  case $(uname -m) in
    x86_64 | amd64) echo x86_64-unknown-linux-musl ;;
    aarch64 | arm64) echo aarch64-unknown-linux-musl ;;
    armv7* | armv8l) echo armv7-unknown-linux-musleabihf ;;
    *) die "Архитектура $(uname -m) не поддерживается." "Unsupported architecture: $(uname -m)." ;;
  esac
}

toml_string() { printf '"%s"' "$(printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g')"; }

install_systemd() {
  can_root || die "Нужны права root (sudo)." "Root (sudo) is required."
  need systemctl || die "systemd не найден: используйте Docker." "systemd not found: use Docker."
  step "Установка сервиса systemd" "Installing the systemd service"
  work=$(mktemp -d)
  trap 'rm -rf "$work"' EXIT
  triple=$(target_triple)
  if [ -n "$binary" ]; then
    # A checkout: the unit sits next to this script.
    mkdir -p "$work/pkg/deploy/systemd"
    cp "$binary" "$work/pkg/mihomyak"
    cp "$(dirname "$0")/systemd/mihomyak.service" "$work/pkg/deploy/systemd/"
  else
    fetch "$RELEASE/mihomyak-$triple.tar.gz" "$work/mihomyak-$triple.tar.gz"
    fetch "$RELEASE/SHA256SUMS" "$work/SHA256SUMS"
    (cd "$work" && grep " mihomyak-$triple.tar.gz\$" SHA256SUMS | sha256sum -c -) >/dev/null \
      || die "Контрольная сумма архива не совпала." "The archive checksum does not match."
    tar xzf "$work/mihomyak-$triple.tar.gz" -C "$work"
    mv "$work"/mihomyak-*-"$triple" "$work/pkg"
  fi
  as_root install -m 755 "$work/pkg/mihomyak" /usr/local/bin/mihomyak
  [ -x /usr/local/bin/mihomo ] || as_root /usr/local/bin/mihomyak core install --dest /usr/local/bin/mihomo
  if [ -f /etc/mihomyak/config.toml ] && [ -z "$url" ]; then
    say "Настройки /etc/mihomyak/config.toml сохранены." "Keeping /etc/mihomyak/config.toml."
  else
    as_root mkdir -p /etc/mihomyak
    as_root chmod 700 /etc/mihomyak
    {
      printf '# mihomyak settings (install.sh). All keys: %s/CONFIG.md\n' "$DOCS"
      printf '# Apply changes: sudo systemctl restart mihomyak\n\n'
      printf '[subscription]\nurl = %s\nclient = %s\nplatform = %s\n\n' "$(toml_string "$url")" \
        "$(toml_string "$client")" "$(toml_string "$platform")"
      printf '[device]\n# The same seed and client, the same device.\nseed = %s\n' "$(toml_string "$seed")"
    } | as_root sh -c 'umask 077; cat > /etc/mihomyak/config.toml'
  fi
  as_root install -m 644 "$work/pkg/deploy/systemd/mihomyak.service" /etc/systemd/system/mihomyak.service
  as_root systemctl daemon-reload
  as_root systemctl enable --now mihomyak >/dev/null
  as_root systemctl restart mihomyak
  printf '%s' "$(t "Жду запуска" "Waiting for the start")"
  i=0
  until as_root /usr/local/bin/mihomyak health >/dev/null 2>&1; do
    i=$((i + 1))
    [ $i -le 90 ] || { printf '\n'; as_root journalctl -u mihomyak -n 20 --no-pager || true
      die "mihomyak не запустился: journalctl -u mihomyak" "mihomyak did not start: journalctl -u mihomyak"; }
    printf .
    sleep 2
  done
  printf ' ok\n'
  as_root /usr/local/bin/mihomyak status || true
  step "Готово" "Done"
  say "Прокси: 127.0.0.1:7890 (HTTP и SOCKS5). Запуск после перезагрузки включён." \
      "Proxy: 127.0.0.1:7890 (HTTP and SOCKS5). It starts after every reboot."
  say "Настройки: /etc/mihomyak/config.toml. Команды: sudo mihomyak status | tui | update" \
      "Settings: /etc/mihomyak/config.toml. Commands: sudo mihomyak status | tui | update"
}

# --- update / uninstall ------------------------------------------------------

update() {
  if [ -f /etc/systemd/system/mihomyak.service ]; then
    mode=systemd
    [ -n "$url" ] || url=
    install_systemd
    return
  fi
  docker_cmd
  dir=${dir:-$(installed_dir)}
  [ -n "$dir" ] || die "mihomyak не найден." "mihomyak is not installed."
  (cd "$dir" && $DOCKER compose pull) || say "Не удалось скачать новый образ, оставляю текущий." \
    "Could not pull a new image, keeping the current one."
  (cd "$dir" && $DOCKER compose up -d)
  wait_healthy
  say "Обновлено." "Updated."
}

uninstall() {
  if [ -f /etc/systemd/system/mihomyak.service ]; then
    as_root systemctl disable --now mihomyak 2>/dev/null || true
    as_root rm -f /etc/systemd/system/mihomyak.service /usr/local/bin/mihomyak
    as_root systemctl daemon-reload
    if [ -n "$purge" ] || confirm "Удалить настройки, данные и mihomo (/etc/mihomyak, /var/lib/mihomyak, /usr/local/bin/mihomo)?" \
      "Delete the settings, data and mihomo (/etc/mihomyak, /var/lib/mihomyak, /usr/local/bin/mihomo)?" n; then
      as_root rm -rf /etc/mihomyak /var/lib/mihomyak /usr/local/bin/mihomo
    fi
    say "Удалено." "Removed."
    return
  fi
  docker_cmd
  dir=${dir:-$(installed_dir)}
  [ -n "$dir" ] || die "mihomyak не найден." "mihomyak is not installed."
  if [ -n "$purge" ] || confirm "Удалить и данные (устройство, кэш подписки) и папку $dir?" \
    "Also delete the data (device, subscription cache) and $dir?" n; then
    (cd "$dir" && $DOCKER compose down -v)
    as_root rm -rf "$dir"
  else
    (cd "$dir" && $DOCKER compose down)
  fi
  if [ -f /usr/local/bin/mihomyak ] && grep -q 'install.sh' /usr/local/bin/mihomyak; then
    as_root rm -f /usr/local/bin/mihomyak
  fi
  say "Удалено." "Removed."
}

# --- main --------------------------------------------------------------------

case $action in
  update) update; exit ;;
  uninstall) uninstall; exit ;;
esac

step "mihomyak" "mihomyak"
if [ -z "$mode" ]; then
  say "Как запустить:" "How to run it:"
  say "  1) шлюз: контейнеры ходят в интернет только через VPN (Docker)" \
      "  1) gateway: containers reach the internet only through the VPN (Docker)"
  say "  2) прокси: HTTP/SOCKS5 для контейнеров любых проектов (Docker)" \
      "  2) proxy: HTTP/SOCKS5 for containers of any project (Docker)"
  say "  3) сервис systemd без Docker: прокси на 127.0.0.1:7890" \
      "  3) systemd service without Docker: a proxy on 127.0.0.1:7890"
  ask choice "Режим" "Mode" 1
  case $choice in 2 | proxy) mode=proxy ;; 3 | systemd) mode=systemd ;; *) mode=gateway ;; esac
fi
case $mode in gateway | proxy | systemd) ;; *) die "Неизвестный режим: $mode" "Unknown mode: $mode" ;; esac

keep_settings=
if [ -z "$url" ]; then
  if [ "$mode" = systemd ] && [ -f /etc/mihomyak/config.toml ]; then keep_settings=1; fi
  if [ "$mode" != systemd ] && need docker && [ -n "$(docker inspect mihomyak 2>/dev/null | head -c1)" ]; then
    keep_settings=1
  fi
fi
if [ -z "$keep_settings" ]; then
  [ -n "$url" ] || ask url "Ссылка на подписку" "Subscription link" ""
  case $url in http://* | https://*) ;; *) die "Нужна ссылка вида https://…" "A https://… link is required." ;; esac
  if [ -z "$client" ]; then
    say "Клиент: flclashx — если провайдер пускает Linux; happ — если он принимает Happ (под Windows или Android)." \
        "Client: flclashx if the provider accepts Linux; happ if it accepts Happ (on Windows or Android)."
    ask client "Клиент, которым представляться (flclashx, koala, happ)" \
      "Client to impersonate (flclashx, koala, happ)" flclashx
  fi
  case $client in flclashx | koala | happ) ;; *) die "Неизвестный клиент: $client" "Unknown client: $client" ;; esac
  if [ "$client" = happ ] && [ -z "$platform" ]; then
    ask platform "Платформа Happ (windows, android, linux)" "Happ platform (windows, android, linux)" windows
  fi
  platform=${platform:-linux}
  case $platform in linux | windows | android) ;; *) die "Неизвестная платформа: $platform" "Unknown platform: $platform" ;; esac
  if [ "$platform" != linux ] && [ "$client" != happ ]; then
    die "Платформа $platform есть только у клиента happ." "Platform $platform exists only for client happ."
  fi
  if [ -z "$seed" ]; then
    say "Seed — любое слово или фраза, из него получается устройство. Тот же seed — то же устройство у провайдера на любом сервере." \
        "The seed is any word or phrase the device is derived from. The same seed is the same device on any server."
    ask seed "Seed (Enter — случайный)" "Seed (Enter for a random one)" ""
    if [ -z "$seed" ]; then
      seed=$(od -An -N12 -tx1 /dev/urandom | tr -d ' \n')
      say "Seed: $seed — сохраните его, чтобы переносить устройство." \
          "Seed: $seed — keep it to move the device later."
    fi
  fi
fi

if [ "$mode" = systemd ]; then install_systemd; else install_docker; fi
