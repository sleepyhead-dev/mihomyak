# mihomyak в Docker

Образ `ghcr.io/sleepyhead-dev/mihomyak` скачивается без регистрации и работает на
`linux/amd64`, `linux/arm64` и `linux/arm/v7`. Внутри mihomyak, mihomo, geo-базы и
iptables; всё состояние лежит в томе `/data`.

| Тег | Что это |
|-----|---------|
| `latest`, `X.Y.Z`, `X.Y` | релизы |
| `edge`, `sha-<commit>` | последняя сборка ветки `main` |

Проще всего поставить установщиком из [README](../README.md#установка): он кладёт в
`/opt/mihomyak` файл `compose.yml` (шлюз или прокси из [`deploy/docker/`](../deploy/docker)) и
`.env` с настройками. Все команды ниже выполняются в этой папке. Переменные `.env` —
в [CONFIG.md](CONFIG.md), [пример со всеми](../deploy/docker/.env.example). Держите `.env`
с правами `600`: в нём ссылка на подписку.

## Шлюз для контейнеров

[`compose.gateway.yml`](../deploy/docker/compose.gateway.yml). Контейнер с
`network_mode: service:mihomyak` не имеет своей сети: он живёт в сетевом пространстве
mihomyak. Там mihomo поднимает TUN-интерфейс и забирает весь трафик и DNS, поэтому
приложению не нужны никакие настройки прокси. Собственный трафик mihomo к VPN-узлам и
запросы mihomyak к панели идут мимо туннеля: подписка обновляется, даже если все узлы
умерли.

```yaml
  my-app:
    image: my/app
    network_mode: service:mihomyak
    depends_on:
      mihomyak: {condition: service_healthy, restart: true}
    restart: unless-stopped
```

- **Порты** приложений публикуются на сервисе `mihomyak`: у них общая сеть.
- **Контейнеры той же Docker-сети** доступны напрямую. Остальной трафик идёт по правилам
  подписки, то есть обычно через VPN.
- **Российские сайты** можно пустить напрямую, мимо VPN: `MIHOMYAK_RULES_PRESETS=ru-direct`
  в `.env`. По умолчанию весь трафик идёт через VPN.

### Kill switch

Пока mihomo не работает (первый старт до получения подписки, перезапуск после сбоя),
TUN нет. В `compose.gateway.yml` kill switch включён: в это время трафик приложений
отклоняется, а не уходит напрямую. Пропускаются только DNS, частные сети и запросы
самого mihomyak к панели. Выключить: `MIHOMYAK_KILL_SWITCH=0`. Подробности — в
[CONFIG.md](CONFIG.md#kill-switch).

### DNS: почему у шлюза обязателен `dns:`

В сетях, которые создаёт compose, контейнеры спрашивают встроенный DNS Docker
(`127.0.0.11`). Имена, которых он не знает, он пересылает дальше, и тут ловушка:
серверы, унаследованные от хоста, он опрашивает **из сети хоста**, мимо туннеля.
Приложения получили бы настоящие адреса от DNS провайдера, который видел бы все имена.
Серверы, заданные явно (`dns:` в compose, `--dns`), Docker опрашивает изнутри
контейнера, где запросы перехватывает mihomo.

Поэтому у сервиса `mihomyak` задан `dns: [1.1.1.1, 8.8.8.8]`. Подойдёт любой сервер,
в том числе роутер: пока mihomo работает, отвечает он сам, а сервер нужен только пока
mihomo не запущен. Если `dns:` убрать, шлюз **откажется стартовать** и объяснит почему.
Принять утечку осознанно можно через `MIHOMYAK_ALLOW_DNS_LEAK=1`.

### Перезапуск и обновление шлюза

Сеть принадлежит конкретному экземпляру контейнера `mihomyak`. Когда он пересоздаётся
или перезапускается, у него новая сеть, а приложения остаются в старой, где нет ничего:
они не видят интернет, пока их не перезапустят. Утечки при этом нет.

- `depends_on: … restart: true` у приложения: compose перезапустит его сам после
  `docker compose restart mihomyak` (нужен Compose 2.17+).
- Обновление образа: `mihomyak upgrade` или
  `docker compose pull && docker compose up -d` **без имени сервиса** — compose
  пересоздаст шлюз и всех, кто от него зависит. `docker compose up -d mihomyak`
  пересоздаст только шлюз.
- Если Docker сам перезапустил упавший контейнер (restart policy), приложения нужно
  перезапустить вручную. Падение mihomo контейнер не перезапускает: mihomyak поднимает
  ядро внутри той же сети за секунду.

## Прокси для контейнеров

[`compose.proxy.yml`](../deploy/docker/compose.proxy.yml). mihomyak слушает HTTP и
SOCKS5 на `mihomyak:7890` в сети `mihomyak`. Приложения указывают его в переменных:

```yaml
  my-app:
    image: my/app
    environment:
      HTTP_PROXY: http://mihomyak:7890
      HTTPS_PROXY: http://mihomyak:7890
      ALL_PROXY: socks5h://mihomyak:7890     # socks5h: DNS тоже через VPN
      NO_PROXY: localhost,127.0.0.1
    networks: [proxy]

networks:
  proxy:
    external: true
    name: mihomyak
```

Так можно подключать контейнеры из **любых** compose-проектов, а перезапуск и
обновление прокси их не ломают. Работает для всего, что уважает эти переменные (curl,
Python, Node, Go и большинство HTTP-клиентов); «сырой» TCP/UDP мимо прокси не пойдёт.

Подключиться к прокси может любой контейнер хоста (`172.16.0.0/12` — частная сеть).
Если на сервере чужие контейнеры, задайте пароль: `MIHOMYAK_PROXY_AUTH=user:пароль`,
а в приложениях `http://user:пароль@mihomyak:7890`.

## Несколько проектов

| Как | Когда | Плюсы | Минусы |
|-----|-------|-------|--------|
| **Шлюз в каждом проекте** с общим `MIHOMYAK_DEVICE_SEED` | нужен прозрачный VPN для любого трафика | обновления и перезапуски безопасны; один слот устройства у провайдера | ~40 МБ памяти на каждый шлюз |
| **Общий прокси** (`compose.proxy.yml`) | приложения ходят по HTTP (боты, парсеры, API) | один контейнер на все проекты, перезапуски ничего не ломают | нужны переменные прокси в приложениях |
| **Общий шлюз**: проекты цепляются через `network_mode: container:mihomyak` | прозрачный VPN, один контейнер | один контейнер и слот | после обновления шлюза все проекты надо пересоздать самому: `docker compose up -d --force-recreate` в каждом |

Общий seed означает, что провайдер видит одно устройство, сколько бы шлюзов ни
работало. Вставьте в `.env` каждого проекта одну и ту же строку
`MIHOMYAK_DEVICE_SEED=длинная-случайная-фраза` и одну и ту же ссылку.

## Устройство и перенос на другой сервер

Провайдер узнаёт устройство по HWID. mihomyak выводит его из machine-id:

- задан `MIHOMYAK_DEVICE_SEED` — machine-id считается из фразы, и устройство одно и
  то же на любом сервере и после удаления тома;
- иначе machine-id генерируется при первом старте и хранится в томе (`/data/machine-id`).
  Потеряете том — получите новое устройство. Перенести: `docker cp mihomyak:/data/machine-id .`
  и `MIHOMYAK_MACHINE_ID=<содержимое>` на новом сервере.

`docker compose exec mihomyak mihomyak identity` показывает HWID и заголовки, которые
увидит панель.

## Шлюз для хоста или LAN (продвинутый)

`network_mode: host` с `MIHOMYAK_GATEWAY=1` заворачивает в VPN **весь хост**. Чтобы
через него ходили другие устройства LAN, укажите хост шлюзом по умолчанию и включите
форвардинг (`sysctl -w net.ipv4.ip_forward=1`). Kill switch в этом режиме защищает
только трафик самого хоста. **Не проверено**: перед боевым использованием проверьте на
своей сети, в том числе доступ к хосту по SSH.

## Точность имитации

В контейнере дистрибутив — Alpine, а hostname — id контейнера. Чтобы заголовки
выглядели как с обычного компьютера, передайте os-release хоста и имя:

```yaml
    hostname: my-laptop                       # Happ: X-Device-Model
    volumes:
      - /etc/os-release:/etc/os-release:ro
```

На armv7 используйте `flclashx` или `koala`: у Happ нет сборки для 32-битного ARM.

## Безопасность

Compose-файлы уже содержат: файловая система только для чтения (писать можно лишь в
`/data`), все capability сброшены, кроме `NET_ADMIN` у шлюза, `no-new-privileges`,
лимиты памяти и процессов, неопубликованный прокси-порт, API mihomo только на
`127.0.0.1` с секретом, файлы с правами `0600`. mihomo получает очищенное окружение без
ссылки и секретов. Из подписки берётся только белый список ключей: провайдер не может
открыть порты или туннели ([CONFIG.md](CONFIG.md#что-mihomyak-делает-с-конфигом-провайдера)).

Лимит `mem_limit` работает, только если в ядре включён memory cgroup. На Raspberry Pi
OS он часто выключен (`cgroup_disable=memory` в `/proc/cmdline`): тогда добавьте
`cgroup_enable=memory` в `/boot/firmware/cmdline.txt` и перезагрузитесь.

## Управление

```sh
docker exec -it mihomyak mihomyak tui
docker exec mihomyak mihomyak status
docker exec mihomyak mihomyak update          # или: docker kill -s HUP mihomyak
docker exec mihomyak mihomyak select PROXY nl
docker logs -f mihomyak
```

Веб-дашборд mihomo не входит в образ. Если он нужен, добавьте в `[mihomo]` ключи
`external-ui`/`external-ui-url`, откройте `core.controller` (секрет обязателен) и
опубликуйте порт только на доверенный интерфейс.

## Сборка образа

```sh
docker build -t mihomyak .                          # из исходников
docker buildx build --platform linux/arm64,linux/amd64 -t mihomyak .
```

Сборке нужен доступ к Docker Hub, crates.io и `dl-cdn.alpinelinux.org`. Чтобы compose
собирал образ из репозитория, добавьте сервису `build: ../..`.
