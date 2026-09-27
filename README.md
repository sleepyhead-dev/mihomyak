<div align="center">

# mihomyak

**VPN-подписка для сервера: прокси и прозрачный шлюз для Docker-контейнеров**

[![CI](https://github.com/sleepyhead-dev/mihomyak/actions/workflows/ci.yml/badge.svg)](https://github.com/sleepyhead-dev/mihomyak/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/sleepyhead-dev/mihomyak)](https://github.com/sleepyhead-dev/mihomyak/releases)
[![Platforms](https://img.shields.io/badge/platforms-amd64%20%7C%20arm64%20%7C%20armv7-blue)](#установка)
[![License: MIT](https://img.shields.io/badge/license-MIT-green)](LICENSE)

Русский · [English](README.en.md)

</div>

mihomyak берёт вашу подписку из панели провайдера (Remnawave, Marzban, PasarGuard,
3x-ui), представляется ей настоящим клиентом (FlClashX, Koala Clash или Happ) и держит
запущенным ядро [mihomo](https://github.com/MetaCubeX/mihomo). Получается VPN для
домашнего сервера, Raspberry Pi или VPS: HTTP/SOCKS-прокси для программ и прозрачный
шлюз для Docker-контейнеров, которым не нужно ничего знать о прокси.

```
панель провайдера ──подписка──▶ mihomyak ──проверенный конфиг──▶ mihomo ──▶ VPN-узлы ──▶ интернет
                                                                     ▲
             ваши контейнеры (network_mode: service:mihomyak) ───────┘  весь трафик и DNS
```

## Что умеет

- **Выглядит как настоящий клиент.** Запрос подписки побайтно совпадает с FlClashX,
  Koala Clash или Happ: User-Agent, HWID, заголовки устройства, их порядок и регистр.
  Устройство задаётся фразой-seed и переживает переустановку.
- **Не ломает рабочий VPN.** Заглушки «Лимит устройств», «Подписка истекла», ошибки
  панели и конфиги, которые не проходят проверку `mihomo -t`, не заменяют рабочий
  конфиг. После перезапуска сеть поднимается сразу из кэша.
- **Шлюз для контейнеров.** Любой контейнер в сети шлюза ходит через VPN, включая
  DNS. Kill switch блокирует трафик, пока VPN не работает, а не пускает его напрямую.
- **Сам обновляется.** По интервалу провайдера, по cron и при старте.
- **Узлы по вкусу.** Фильтры по именам, группы автопереключения (`fallback`,
  `url-test`), по желанию российские сайты напрямую (`ru-direct`).
- **Лёгкий и безопасный.** 2–3 МБ памяти (плюс ~40 МБ у mihomo), один статический
  бинарник. Подписка считается недоверенной: провайдер не может открыть порты на вашем
  сервере. Контейнер работает с минимальными правами.
- **Удобный.** Команды `status`, `select`, `test` и интерактивный `tui` в терминале.

## Установка

Нужны Linux и подписка — ссылка вида `https://…/sub/…` из бота или личного кабинета
провайдера. Поддерживаются `amd64`, `arm64` (Raspberry Pi 3/4/5 с 64-битной ОС) и
`armv7`. Для режимов с Docker нужен установленный
[Docker](https://docs.docker.com/engine/install/).

```sh
curl -fsSL https://github.com/sleepyhead-dev/mihomyak/releases/latest/download/install.sh | sh
```

Установщик спросит ссылку, режим и seed устройства, а остальное сделает сам:

- **шлюз** (по умолчанию): контейнеры, подключённые к нему, ходят в интернет только
  через VPN, включая DNS;
- **прокси**: HTTP и SOCKS5 на `mihomyak:7890` для контейнеров любых compose-проектов;
- **без Docker**: сервис systemd с прокси на `127.0.0.1:7890`.

Он создаёт `/opt/mihomyak` с настройками (`.env`, права `600`), запускает mihomyak,
ждёт, пока тот заработает, включает автозапуск после перезагрузки и ставит команду
`mihomyak` (`mihomyak status`, `mihomyak tui`, `mihomyak logs`, `mihomyak upgrade`).
В конце он покажет, как подключить свой контейнер.

Seed — любое слово или фраза (например, `apple`), из которой получается устройство:
тот же seed — то же устройство у провайдера на любом сервере. Enter — случайный seed.

Без вопросов, например для скриптов:

```sh
curl -fsSL https://github.com/sleepyhead-dev/mihomyak/releases/latest/download/install.sh \
  | sh -s -- --yes --url 'https://…/sub/…' --mode gateway --seed apple
```

Обновить: `mihomyak upgrade`. Удалить: `mihomyak uninstall`.

### Подключить свой контейнер к шлюзу

Добавьте сервис в `/opt/mihomyak/compose.yml` и выполните `docker compose up -d` в этой
папке:

```yaml
  my-app:
    image: my/app
    network_mode: service:mihomyak        # вся сеть приложения — через VPN
    depends_on:
      mihomyak: {condition: service_healthy, restart: true}
    restart: unless-stopped
```

Порты приложения публикуются на сервисе `mihomyak`. Как подключить проекты из других
папок и что выбрать для нескольких проектов — в [docs/DOCKER.md](docs/DOCKER.md).

<details>
<summary>Установка вручную</summary>

Docker-шлюз (для прокси — `compose.proxy.yml`):

```sh
sudo mkdir -p /opt/mihomyak && sudo chown "$USER" /opt/mihomyak && cd /opt/mihomyak
curl -fsSL -o compose.yml https://github.com/sleepyhead-dev/mihomyak/releases/latest/download/compose.gateway.yml
curl -fsSL -o .env https://github.com/sleepyhead-dev/mihomyak/releases/latest/download/env.example
chmod 600 .env && nano .env                 # ссылка в MIHOMYAK_SUB_URL, seed
docker compose up -d
docker compose exec mihomyak mihomyak status
```

Без Docker (бинарник под свою архитектуру: `x86_64`, `aarch64`, `armv7`):

```sh
curl -fsSL https://github.com/sleepyhead-dev/mihomyak/releases/latest/download/mihomyak-aarch64-unknown-linux-musl.tar.gz | tar xz
sudo install -m 755 mihomyak-*/mihomyak /usr/local/bin/
sudo mihomyak core install --dest /usr/local/bin/mihomo
sudo install -D -m 600 mihomyak-*/deploy/config.example.toml /etc/mihomyak/config.toml
sudoedit /etc/mihomyak/config.toml          # ссылка в [subscription] url
sudo cp mihomyak-*/deploy/systemd/mihomyak.service /etc/systemd/system/
sudo systemctl enable --now mihomyak
```

</details>

## Команды

`mihomyak --help` показывает все команды по группам, `mihomyak <команда> --help` —
подробности. Команду `mihomyak` на хосте ставит установщик; без неё в Docker —
`docker exec mihomyak mihomyak <команда>`. Ещё две команды есть только на хосте:
`mihomyak logs` и `mihomyak upgrade` (обновить образ).

| Команда | Что делает |
|---------|------------|
| `status` | подписка, трафик, срок, следующее обновление, выбранные узлы |
| `tui` | интерактивный экран: узлы, задержки, выбор стрелками (`docker exec -it …`) |
| `proxies [группа]` | группы или узлы группы с задержками |
| `select <группа> <узел>` | выбрать узел (хватит части имени: `select proxy nl`) |
| `test [группа]` | замерить задержки |
| `mode [rule\|global\|direct]` | режим маршрутизации |
| `update` | обновить подписку сейчас (код выхода 1, если панель отказала) |
| `fetch` | диагностика: что ушло в панель, что она ответила и почему |
| `identity` | каким устройством mihomyak представляется, его HWID |
| `check` / `render` | проверить настройки / показать итоговый конфиг mihomo |
| `run` | сам сервис (точка входа контейнера и systemd) |

## Настройка

Настройки Docker-установки — в `/opt/mihomyak/.env`; после правки выполните
`docker compose up -d` в этой папке. Основные переменные:

| Переменная | Что задаёт |
|------------|------------|
| `MIHOMYAK_SUB_URL` | ссылка на подписку |
| `MIHOMYAK_CLIENT` | какой клиент изображать: `flclashx` (по умолчанию), `koala`, `happ` |
| `MIHOMYAK_DEVICE_SEED` | фраза, из которой получается устройство (HWID). Тот же seed — то же устройство на любом сервере |
| `MIHOMYAK_UPDATE_CRON` | расписание обновления, например `0 5 * * *` (время по `TZ`) |
| `MIHOMYAK_RULES_PRESETS` | `ru-direct` — российские сайты напрямую, мимо VPN (по умолчанию весь трафик через VPN) |
| `MIHOMYAK_EXCLUDE` | убрать узлы по именам: `*Россия*;*Info*` |

Всё остальное (группы автопереключения, свои правила, любые ключи mihomo) задаётся в
`config.toml`: [пример со всеми ключами](deploy/config.example.toml),
[справочник](docs/CONFIG.md).

### Какого клиента изображать

| Клиент | Что отдаёт панель | Когда выбирать |
|--------|-------------------|----------------|
| `flclashx` | mihomo YAML во всех панелях | почти всегда |
| `koala` | YAML или ссылки | если провайдер пускает только Koala |
| `happ` | ссылки или Xray JSON (конвертируется) | если провайдер пускает только Happ |

У каждого клиента своя формула HWID, поэтому смена клиента — это новое устройство у
провайдера. Выберите клиента один раз.

## Если что-то не так

- `mihomyak status` и `docker logs mihomyak` — первое, что стоит посмотреть.
- `mihomyak fetch` показывает запрос к панели, её ответ и вердикт.
- Частые вопросы (лимит устройств, «платформа не поддерживается», DNS, контейнеры
  без сети после перезапуска шлюза) — в [docs/FAQ.md](docs/FAQ.md).

Известные ограничения: TLS-отпечаток запроса отличается от настоящих клиентов (важно,
только если панель за антибот-защитой), ссылки `happ://crypt…` не поддерживаются.

## Документация

- [docs/DOCKER.md](docs/DOCKER.md) — Docker: шлюз, прокси, несколько проектов, обновление;
- [docs/CONFIG.md](docs/CONFIG.md) — все настройки и переменные окружения;
- [docs/FAQ.md](docs/FAQ.md) — частые вопросы и проблемы;
- [docs/dev/](docs/dev/DEVELOPMENT.md) — для разработчиков: сборка, тесты, устройство
  кода, исследование панелей и клиентов.

## Лицензия

[MIT](LICENSE). Проект вырос из форка [mihoro](https://github.com/spencerwooo/mihoro)
и был полностью переписан.
