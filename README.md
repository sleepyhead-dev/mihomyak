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
провайдера. Готовые образы и бинарники есть для `amd64`, `arm64` (Raspberry Pi 3/4/5
с 64-битной ОС) и `armv7`.

### Вариант 1. Docker-шлюз (рекомендуется)

Контейнеры, подключённые к шлюзу, выходят в интернет только через VPN.

```sh
mkdir -p ~/mihomyak && cd ~/mihomyak
curl -fsSLO https://raw.githubusercontent.com/sleepyhead-dev/mihomyak/main/deploy/docker/compose.gateway.yml
curl -fsSL -o .env https://raw.githubusercontent.com/sleepyhead-dev/mihomyak/main/deploy/docker/.env.example
chmod 600 .env
nano .env                                   # вставьте ссылку в MIHOMYAK_SUB_URL
docker compose -f compose.gateway.yml up -d
docker compose -f compose.gateway.yml exec mihomyak mihomyak status
```

Шлюз запускается сам после перезагрузки (`restart: unless-stopped`), если включён
автозапуск Docker (`sudo systemctl enable docker`, обычно он уже включён).

Свой контейнер подключается одной строкой в том же `compose.gateway.yml`:

```yaml
  my-app:
    image: my/app
    network_mode: service:mihomyak        # вся сеть приложения — через VPN
    depends_on:
      mihomyak: {condition: service_healthy, restart: true}
```

Порты приложения публикуются на сервисе `mihomyak`. Как подключить несколько
проектов, обновлять шлюз и что делать с DNS — в [docs/DOCKER.md](docs/DOCKER.md).

### Вариант 2. Прокси для контейнеров и программ

Если приложению достаточно `HTTP_PROXY`, возьмите
[`compose.proxy.yml`](deploy/docker/compose.proxy.yml): mihomyak слушает
`http://mihomyak:7890` (HTTP и SOCKS5) в общей Docker-сети, приложения указывают его
в `HTTP_PROXY`/`ALL_PROXY`. Перезапуск прокси приложения не ломает.

### Вариант 3. Без Docker (systemd)

```sh
# бинарник под свою архитектуру: x86_64, aarch64 или armv7
curl -fsSL https://github.com/sleepyhead-dev/mihomyak/releases/latest/download/mihomyak-aarch64-unknown-linux-musl.tar.gz | tar xz
sudo install -m 755 mihomyak-*/mihomyak /usr/local/bin/
sudo mihomyak core install --dest /usr/local/bin/mihomo
sudo install -D -m 600 mihomyak-*/deploy/config.example.toml /etc/mihomyak/config.toml
sudoedit /etc/mihomyak/config.toml          # вставьте ссылку в [subscription] url
sudo cp mihomyak-*/deploy/systemd/mihomyak.service /etc/systemd/system/
sudo systemctl enable --now mihomyak        # запуск сейчас и после каждой перезагрузки
mihomyak status
```

Прокси будет на `127.0.0.1:7890`. Чтобы проксировать весь хост, включите
`[gateway] enable = true` ([docs/CONFIG.md](docs/CONFIG.md#gateway--прозрачный-шлюз-tun)).

## Команды

`mihomyak --help` показывает все команды по группам, `mihomyak <команда> --help` —
подробности. В Docker: `docker exec mihomyak mihomyak <команда>`.

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

Для Docker достаточно `.env` рядом с compose-файлом. Основные переменные:

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
