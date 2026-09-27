# mihomyak

Лёгкий CLI/TUI-клиент на [mihomo](https://github.com/MetaCubeX/mihomo) для Linux
(x86_64, arm64, armv7), рассчитанный на подписки, популярные в СНГ: Remnawave,
Marzban, PasarGuard, 3x-ui. Его задача — стабильно получать подписку и держать
прокси для домашнего сервера или VPS, в Docker или без него.

- **Точная имитация клиентов.** Запрос подписки совпадает побайтно с FlClashX 0.4.2,
  Koala Clash 1.4.1 или Happ Desktop 4.3.0: User-Agent, HWID, `x-device-*`, порядок
  и регистр заголовков. Всё выверено по исходникам и перехваченным запросам:
  [docs/SUBSCRIPTIONS.md](docs/SUBSCRIPTIONS.md).
- **Заглушки и битые конфиги не ломают прокси.** Ответы «App not supported»,
  «Лимит устройств» и «Подписка истекла» (серверы `0.0.0.0:1`, заголовки `x-hwid-*`)
  распознаются и не заменяют рабочий конфиг. Каждый новый конфиг проверяется
  `mihomo -t`, а если ядро всё же его отвергло, возвращается предыдущий.
- **Любой формат подписки:** mihomo YAML, base64/ссылки (`vless`, `vmess`, `trojan`,
  `ss`, `hy2`, …), Xray JSON (конвертируется в прокси mihomo).
- **Автообновление:** интервал (из `profile-update-interval` или свой), cron в
  локальном времени, обновление при старте. Пропущенные запуски догоняются.
- **Узлы по именам:** белый и чёрный списки, группы автопереключения
  (`fallback` / `url-test` / `load-balance`) с приоритетом по маскам.
- **Шлюз для контейнеров:** TUN с перехватом DNS. Контейнеры с
  `network_mode: service:mihomyak` проксируются прозрачно.
- **Лёгкость:** супервизор без async-рантайма занимает около 2–4 МБ RSS, бинарник
  около 3 МБ (static musl). Всё остальное потребление — это сам mihomo.
- **Безопасность по умолчанию:** подписка считается недоверенной (из неё берётся
  только белый список ключей, провайдер не может открыть порты или туннели на вашем
  сервере), прокси-порт только на loopback, LAN — только из частных сетей,
  опциональный пароль, секрет API, файлы с правами `0600`, hardening в compose и
  systemd.

## Быстрый старт: Docker-шлюз

```sh
git clone https://github.com/sleepyhead-dev/mihomyak && cd mihomyak/deploy
cp .env.example .env && chmod 600 .env && $EDITOR .env   # MIHOMYAK_SUB_URL=…
docker compose -f compose.gateway.yml up -d               # или up -d --build
docker compose -f compose.gateway.yml exec mihomyak mihomyak status
```

Образ `ghcr.io/sleepyhead-dev/mihomyak` собирает CI (`latest` — последний релиз,
`edge` — ветка `main`). Пока репозиторий приватный, перед `pull` нужен
`docker login ghcr.io` с токеном `read:packages`. Либо соберите образ на месте:
`docker compose … up -d --build`.

Любой контейнер с `network_mode: service:mihomyak` ходит в сеть через прокси, включая
DNS. Для DNS у сервиса mihomyak должен быть явный `dns:` (он уже есть в
`compose.gateway.yml`), иначе Docker резолвит имена приложений мимо туннеля
([подробности](docs/DOCKER.md#сценарий-1-прозрачный-шлюз-для-контейнеров-рекомендуется)).
Другие сценарии (явный HTTP/SOCKS-прокси, шлюз для LAN, ARM) описаны в
[docs/DOCKER.md](docs/DOCKER.md).

## Без Docker

Готовые статические бинарники для x86_64, aarch64 и armv7 лежат в
[релизах](https://github.com/sleepyhead-dev/mihomyak/releases) (с `SHA256SUMS`).
Или соберите сами:

```sh
./scripts/build-static.sh aarch64-unknown-linux-musl     # или make static
sudo install -m 755 target/aarch64-unknown-linux-musl/release/mihomyak /usr/local/bin/
sudo mihomyak core install --dest /usr/local/bin/mihomo  # mihomo с GitHub
sudo install -D -m 600 examples/config.toml /etc/mihomyak/config.toml  # вписать url
sudo cp contrib/mihomyak.service /etc/systemd/system/ && sudo systemctl enable --now mihomyak
```

## Команды

`mihomyak --help` показывает команды по группам с примерами,
`mihomyak <команда> --help` — подробности.

| Команда | Что делает |
|---------|------------|
| `mihomyak run` | супервизор: держит mihomo запущенным и обновляет подписку (точка входа контейнера) |
| `mihomyak update` | обновить подписку сейчас (сигнал работающему супервизору) |
| `mihomyak status` | трафик, срок, следующее обновление, выбранные узлы |
| `mihomyak proxies [группа]` | группы или узлы группы с задержками |
| `mihomyak select <группа> <узел>` | выбрать узел (имена можно сокращать: `select remna nl`) |
| `mihomyak test [группа]` | замер задержек |
| `mihomyak mode [rule\|global\|direct]` | режим маршрутизации (сохраняется) |
| `mihomyak tui` | интерактивный интерфейс |
| `mihomyak fetch [--client happ]` | диагностика: какие заголовки ушли, что ответила панель, вердикт |
| `mihomyak identity` | эмулируемое устройство, HWID и точные заголовки |
| `mihomyak check` / `render` | проверить настройки и собранный из кэша конфиг через `mihomo -t` / показать итоговый конфиг mihomo |
| `mihomyak health` | healthcheck для Docker |
| `mihomyak core install` | скачать mihomo под текущую архитектуру (`--sha256`; с `--mirror` он обязателен) |

## Настройка

Всё задаётся в `config.toml` ([пример со всеми ключами](examples/config.toml)) или
переменными `MIHOMYAK_*` ([справочник](docs/CONFIG.md)). Настройки читаются при
старте, после изменений перезапустите супервизор. Фрагмент:

```toml
[subscription]
url = "https://sub.example.com/…"
client = "flclashx"

[update]
cron = ["0 5 * * *"]     # каждый день в 05:00 по TZ
on_start = true          # и при каждом старте контейнера

[filter]
exclude = ["*Россия*", "*Info*"]

[[groups]]
name = "Auto"
type = "fallback"
nodes = ["*🇳🇱*", "*🇩🇪*"]   # порядок = приоритет
default = true

[rules]
presets = ["ru-direct"]
```

## Какого клиента имитировать

| Клиент | Что отдаст панель | Когда выбирать |
|--------|-------------------|----------------|
| `flclashx` (по умолчанию) | mihomo YAML во всех панелях | почти всегда |
| `koala` | YAML в Remnawave/PasarGuard, ссылки в Marzban | если провайдер пускает только Koala |
| `happ` | ссылки или Xray JSON (конвертируется) | если провайдер пускает только Happ |

Один machine-id даёт **разные HWID** у разных клиентов. Смена клиента занимает новый
слот устройства у провайдера, поэтому выбирайте один раз. Чтобы переиспользовать уже
зарегистрированное устройство, задайте `MIHOMYAK_MACHINE_ID` или `MIHOMYAK_HWID`.

## Ограничения

- В режиме шлюза, пока mihomo не запущен (старт, перезапуск после падения), трафик
  контейнеров по умолчанию идёт напрямую. Чтобы в это время он блокировался,
  включите `MIHOMYAK_KILL_SWITCH=1` (`gateway.kill_switch`), см.
  [docs/DOCKER.md](docs/DOCKER.md).
- TLS-отпечаток запроса (JA3/JA4) отличается от настоящих клиентов (rustls). Это
  важно, только если панель стоит за антибот-защитой, проверяющей отпечаток.
- Зашифрованные ссылки `happ://crypt…` не поддерживаются. Домены вроде `.рф`
  переводятся в punycode автоматически, как в браузере.

## Разработка

```sh
make check        # fmt, clippy (с TUI и без), тесты, rustdoc — как в CI
make e2e          # собрать образ и прогнать контейнеры против мок-панели
make static TARGET=armv7-unknown-linux-musleabihf
```

CI (GitHub Actions) на каждый PR и push в `main`: линтеры, тесты (с проверкой
конфигов настоящим `mihomo -t`), MSRV 1.88, статические сборки под три архитектуры
(скачиваются как артефакты), аудит зависимостей, e2e в Docker. Push в `main`
публикует образ `:edge`. Релиз выпускается тегом `vX.Y.Z` (совпадающим с версией в
`Cargo.toml`) или кнопкой Actions → Release → Run workflow (тег создаётся сам):
бинарники, `SHA256SUMS`, заметки из `CHANGELOG.md` и образ `:X.Y.Z`/`:latest`. Подробности для разработчиков и AI-агентов — в
[AGENTS.md](AGENTS.md).

## Документация

- [docs/SUBSCRIPTIONS.md](docs/SUBSCRIPTIONS.md): как работают подписки, HWID,
  заглушки, точные запросы клиентов (исследование);
- [docs/CONFIG.md](docs/CONFIG.md): все настройки и переменные окружения;
- [docs/DOCKER.md](docs/DOCKER.md): сценарии развёртывания, шлюз, hardening;
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): устройство кода и принятые решения;
- [AGENTS.md](AGENTS.md): как продолжать разработку (для людей и AI-агентов).

## Лицензия

MIT. Проект начинался как форк [mihoro](https://github.com/spencerwooo/mihoro)
и затем был переписан.
