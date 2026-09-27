# Docker

Образ: `ghcr.io/sleepyhead-dev/mihomyak` (`linux/amd64`, `linux/arm64`, `linux/arm/v7`).
База — официальный образ `metacubex/mihomo`: mihomo, CA-сертификаты, tzdata и
geo-базы. Бинарник mihomyak кросс-компилируется (`tonistiigi/xx`) без QEMU.

| Путь / переменная | Значение |
|-------------------|----------|
| `/data` (volume) | всё состояние: machine-id (HWID!), секрет, кэш подписки, конфиг и кэш mihomo |
| `/data/config.toml` | необязательный конфиг (`MIHOMYAK_CONFIG`) |
| `MIHOMYAK_CORE_BIN=/mihomo` | mihomo из базового образа |
| `MIHOMYAK_GEODATA_DIR=/root/.config/mihomo` | недостающие geo-базы копируются в `/data/mihomo` при старте |
| порт `7890` | HTTP+SOCKS5 (слушает только loopback, пока не задан `MIHOMYAK_ALLOW_LAN=1`) |
| `HEALTHCHECK` | `mihomyak health` (отвечает ли API mihomo) |

**Volume `/data` нельзя терять:** в нём machine-id. Новый machine-id — это новый HWID
и новый слот устройства у провайдера.

## Сценарий 1. Прозрачный шлюз для контейнеров (рекомендуется)

[`deploy/compose.gateway.yml`](../deploy/compose.gateway.yml)

```yaml
services:
  mihomyak:
    image: ghcr.io/sleepyhead-dev/mihomyak:latest
    env_file: .env                 # deploy/.env: MIHOMYAK_SUB_URL и прочее
    environment: { MIHOMYAK_GATEWAY: "1" }
    dns: [1.1.1.1, 8.8.8.8]        # обязательно, см. «DNS приложений» ниже
    devices: [/dev/net/tun:/dev/net/tun]
    cap_drop: [ALL]
    cap_add: [NET_ADMIN]
    volumes: [mihomyak-data:/data]
  app:
    image: my/app
    network_mode: service:mihomyak   # весь трафик app — через прокси
```

Как это работает: контейнеры с `network_mode: service:mihomyak` делят сетевое
пространство имён с mihomyak. mihomo поднимает в нём TUN с `auto-route` и перехватом
DNS (`dns-hijack any:53`, fake-ip), поэтому любой TCP/UDP и DNS приложения идёт
через mihomo без настройки прокси в самом приложении. Собственный трафик mihomo
(к серверам прокси) маршрутизируется мимо TUN.

Порты приложений публикуются **на сервисе mihomyak** (у них общий сетевой стек).
Проверено в этой сборке: запрос из контейнера-клиента проходит через
shadowsocks-узел; при остановке узла запрос падает (обхода нет); DNS отдаёт fake-ip.

**DNS приложений.** В сетях, которые создаёт compose (и любых `docker network
create`), контейнеры спрашивают встроенный DNS Docker (`127.0.0.11`). Неизвестные ему
имена он пересылает дальше, и тут есть ловушка: серверы, унаследованные из
`/etc/resolv.conf` хоста, он опрашивает **из сетевого пространства хоста**, мимо
TUN. Тогда приложения получают настоящие адреса от DNS хоста или провайдера (имена
видны ему, правила по доменам не срабатывают), а трафик идёт в туннель уже по IP.
Серверы, заданные явно (`dns:` в compose, `--dns`), Docker опрашивает из
пространства контейнера: mihomo перехватывает такие запросы и отвечает fake-ip.
Поэтому у сервиса mihomyak обязательно задайте `dns:`. Подойдёт любой сервер, в том
числе роутер: пока mihomo работает, отвечает он, а сам сервер нужен только пока mihomo
не запущен (первый старт, перезапуск), в том числе чтобы супервизор нашёл панель.
Без `dns:` mihomyak пишет предупреждение при старте и в `mihomyak check` (Docker
отмечает такие серверы в `resolv.conf` как `# ExtServers: [host(…)]`). Сеть `bridge`
по умолчанию (`docker run` без `--network`) не затронута: там DNS хоста копируется в
контейнер и опрашивается изнутри. Проверено на Raspberry Pi 3B+ (Docker 29.8):
без `dns:` `nslookup example.com` в контейнере-клиенте возвращает настоящие адреса, с
`dns:` — `198.18.x.x`.

Сам супервизор тоже живёт в этом пространстве имён, поэтому хосты панели и их
адреса выводятся из-под туннеля (`fake-ip-filter` и `route-exclude-address`):
подписка обновляется, даже если все узлы умерли. Трафик приложений к адресу панели
тоже идёт напрямую.

**Kill switch.** Пока mihomo не работает (первый старт до получения подписки,
перезапуск после падения), TUN нет, и по умолчанию трафик приложений идёт напрямую
через docker-сеть. Если утечка недопустима, добавьте `MIHOMYAK_KILL_SWITCH=1`: тогда
в это время наружу выходят только DNS, трафик самого mihomo и mihomyak и обращения
к частным сетям, остальное отклоняется. Проверено в этой сборке: при убитом mihomo
запрос приложения отклоняется, `mihomyak fetch` до панели проходит, после
перезапуска ядра трафик снова идёт через узел. Подробности —
[CONFIG.md](CONFIG.md#kill-switch).

## Сценарий 2. Явный HTTP/SOCKS-прокси

[`deploy/compose.proxy.yml`](../deploy/compose.proxy.yml): mihomyak и приложения в одной
docker-сети, у приложений `HTTP_PROXY=http://mihomyak:7890`. Нужен
`MIHOMYAK_ALLOW_LAN=1`. Подключения принимаются только из частных сетей, куда входят
docker-сети `172.16.0.0/12`, то есть **любой** контейнер на хосте. Задайте пароль через
`MIHOMYAK_PROXY_AUTH` или сузьте `core.lan_allowed_ips` до своей сети. TUN и
`NET_ADMIN` здесь не нужны.

## Сценарий 3. Шлюз для LAN или хоста (продвинутый)

`network_mode: host` + `MIHOMYAK_GATEWAY=1` проксирует **весь хост**. Чтобы другие
устройства LAN ходили через этот хост, укажите его шлюзом по умолчанию (на роутере
или в DHCP) и включите форвардинг: `sysctl -w net.ipv4.ip_forward=1`. TUN с
`auto-route` заворачивает и форвардируемый трафик. **Не проверено в этой сборке**:
перед боевым использованием проверьте на своей сети, в том числе доступ к хосту
по SSH.

## Hardening

Оба compose-файла уже содержат:

- `read_only: true` + `tmpfs: /tmp`: запись возможна только в `/data`;
- `cap_drop: [ALL]`, и только для шлюза `cap_add: [NET_ADMIN]`;
- `no-new-privileges`;
- `mem_limit: 256m`, `pids_limit: 256` (mihomo обычно занимает ~40 МБ);
- `stop_grace_period: 15s`: супервизор ждёт mihomo до 8 с, чтобы тот убрал маршруты;
- прокси-порт не опубликован наружу; API mihomo только на `127.0.0.1` внутри
  контейнера, с сгенерированным секретом;
- umask 077: секреты и подписка в `/data` с правами `0600`;
- mihomo запускается с очищенным окружением (ссылка на подписку и секреты
  `MIHOMYAK_*` ему не передаются) и завершается вместе с супервизором;
- из подписки берётся только белый список ключей: провайдер не может открыть
  порты, listeners или туннели в контейнере ([CONFIG.md](CONFIG.md#что-mihomyak-делает-с-конфигом-провайдера)).

Проверено: контейнер с `--read-only --cap-drop ALL --cap-add NET_ADMIN
--security-opt no-new-privileges` запускается, становится healthy и проксирует.

`deploy/.env` содержит ссылку на подписку. Держите его с правами `600` и не
коммитьте (`.env` в `.gitignore`).

## Точность имитации в контейнере

В контейнере os-release от Alpine, а hostname — это id контейнера. Чтобы заголовки
выглядели как с обычного десктопа:

```yaml
    hostname: my-laptop                       # Happ: X-Device-Model
    volumes:
      - /etc/os-release:/etc/os-release:ro    # дистрибутив хоста в x-device-model / x-ver-os
```

Либо задайте поля явно: `[device] os_name/os_version/os_pretty_name`.

На armv7 используйте `flclashx` или `koala`: у Happ нет Linux-сборки для 32-битного
ARM, и такой UA выглядел бы неправдоподобно.

## Управление

```sh
docker exec -it mihomyak mihomyak tui
docker exec mihomyak mihomyak status
docker exec mihomyak mihomyak update           # или: docker kill -s HUP mihomyak
docker exec mihomyak mihomyak select PROXY nl
docker logs -f mihomyak
```

Дашборд mihomo (zashboard, metacubexd) не включён. Если нужен, добавьте в
`[mihomo]` ключи `external-ui`/`external-ui-url`, выставьте `core.controller` наружу
(секрет обязателен) и опубликуйте порт только на доверенный интерфейс.

## Образы и теги

| Тег | Откуда |
|-----|--------|
| `latest`, `X.Y.Z`, `X.Y` | релизы (git-тег `vX.Y.Z`) |
| `edge`, `sha-<commit>` | каждый push в `main` |

Платформы: `linux/amd64`, `linux/arm64`, `linux/arm/v7`. Пока репозиторий
приватный, пакет в GHCR тоже приватный: `docker login ghcr.io -u <user>` с
personal access token (право `read:packages`). Либо сделайте пакет публичным в
настройках пакета на GitHub.

## Сборка образа

```sh
docker buildx build --platform linux/arm64,linux/amd64 -t mihomyak .
docker buildx build --build-arg MIHOMO_VERSION=v1.19.31 --load -t mihomyak .
make e2e      # собрать образ и проверить его против мок-панели
```

Сборочной стадии нужен доступ к `dl-cdn.alpinelinux.org` (пакеты clang/lld),
crates.io и Docker Hub. В compose-файлах рядом с `image:` указан `build: ..`: если
образ ещё не опубликован, `docker compose up --build` соберёт его из репозитория.
