# Архитектура

## Процессы

```
┌─────────────── mihomyak run (PID 1 в контейнере, ~2–4 МБ RSS) ───────────────┐
│ поток signals ──(TERM/INT/HUP/CHLD)──▶ главный цикл (recv_timeout)            │
│                                         │                                      │
│   расписание (interval / cron / on_start) ─▶ Updater: fetch → analyze → build │
│                                         │         │                            │
│                                         │         └─▶ /data/mihomo/config.yaml │
│                                         ├─▶ PUT /configs (hot reload) ─────┐   │
│                                         └─▶ spawn / SIGTERM / restart ──┐  │   │
└─────────────────────────────────────────────────────────────────────────┼──┼───┘
                                                                          ▼  ▼
                                                              mihomo (дочерний процесс)
CLI/TUI (mihomyak status/select/tui…) ──REST API 127.0.0.1:9090 + secret──▶ mihomo
mihomyak update ──SIGHUP──▶ супервизор (единственный, кто пишет подписку)
```

Async-рантайма нет. Супервизор почти всё время спит в `recv_timeout`: поток
сигналов, главный цикл и короткие блокирующие HTTP-запросы. tokio добавил бы
мегабайты к бинарнику и RSS, ничего не дав.

## Модули (`src/`)

| Модуль | Ответственность |
|--------|-----------------|
| `main.rs` | umask 077, логгер, разбор CLI, коды выхода |
| `cli.rs` | clap-описание команд |
| `commands.rs` | реализация команд (status, proxies, select, fetch, check…) |
| `config.rs` | TOML + env, валидация, значения по умолчанию |
| `identity.rs` | machine-id, os-release (два парсера: по спецификации и «как регулярка Koala»), hostname, локаль |
| `emulation.rs` | **точные** заголовки FlClashX / Koala / Happ, формулы HWID и UA |
| `http.rs` | свой HTTP/1.1-клиент: заголовки как есть, rustls без ALPN, chunked, gzip/deflate/br/zstd, CONNECT-прокси, unix-сокет |
| `subscription/mod.rs` | загрузка с редиректами, `analyze` → `Problem` (Refused/Http/Invalid/Stub) |
| `subscription/headers.rs` | `subscription-userinfo`, `profile-*`, `announce`, `x-hwid-*`, `flclashx-newdomain` |
| `subscription/body.rs` | формат тела: YAML / ссылки / base64 / Xray JSON / HTML; адреса узлов |
| `subscription/stub.rs` | распознавание заглушек (0.0.0.0/loopback/port ≤ 1, HWID-отказы) |
| `subscription/xray.rs` | Xray JSON → прокси mihomo (маппинг как в генераторе Remnawave) |
| `pattern.rs` | glob-маски имён узлов; перевод в Go-regex для провайдеров |
| `profile.rs` | сборка `config.yaml`: очистка, фильтр, группы, правила, управляемые ключи, TUN, `[mihomo]` |
| `schedule.rs` | cron (5 полей) и локальное время через `localtime_r` |
| `updater.rs` | конвейер обновления, кэш, расписание следующего обновления |
| `supervisor.rs` | цикл `run`: сигналы, запуск и перезапуск mihomo, hot reload, группы по умолчанию, reaping |
| `core.rs` | процесс mihomo (SIGTERM → SIGKILL), поиск бинарника, `core install`, geo-базы |
| `api.rs` | REST API mihomo (proxies, select, delay, configs, connections) |
| `store.rs` | каталог данных: machine-id, secret, pid, mode, кэш подписки, метаданные |
| `tui.rs` | ratatui-интерфейс (feature `tui`) |
| `util.rs`, `log.rs` | время без chrono, размеры, sha256 (ring), атомарная запись, логгер |

## Ключевые решения

1. **Свой HTTP-клиент.** Главное требование — побайтная имитация клиентов. Все
   распространённые клиенты (reqwest, ureq, hyper) проводят заголовки через
   крейт `http`: имена приводятся к нижнему регистру, порядок и неявные заголовки
   задаёт библиотека. У Koala заголовки в смешанном регистре, у FlClashX — свой
   порядок из HashMap Dart. Нужное подмножество HTTP/1.1 небольшое (~500 строк
   вместе с тестами).
2. **mihomo — единственное ядро.** Ссылки разбирает сам mihomo (file-провайдер).
   Xray JSON (ответ панели для Happ) конвертируется по той же таблице полей, что
   использует генератор mihomo в Remnawave. Всё, что не переносится, попадает в лог.
3. **Кэш — источник правды при старте.** Как реальные клиенты, mihomyak не
   дёргает панель при каждом рестарте: конфиг собирается из последней принятой
   подписки, а обновление идёт по расписанию (или сразу, если `on_start`). Сеть
   поднимается мгновенно даже при недоступной панели.
4. **Заглушка никогда не заменяет рабочий конфиг.** `analyze` классифицирует
   ответ; применяется только `usable()`.
5. **Клиент владеет сетью хоста.** Порты, LAN, TUN, контроллер, режим из подписки
   игнорируются: провайдер не должен решать, что открыто на вашем сервере.
6. **Один писатель.** Подписку пишет только супервизор; `mihomyak update` шлёт
   ему SIGHUP. Если супервизор не запущен, команда пишет сама.
7. **Безопасность по умолчанию.** loopback-прокси, `lan-allowed-ips`, секрет API,
   umask 077, файлы 0600, hardening в compose. Подробности — в README и
   [DOCKER.md](DOCKER.md).
8. **Минимум зависимостей.** Никаких chrono, regex, tokio, reqwest. Криптография —
   ring (уже нужен rustls); zstd и brotli — на чистом Rust.

## Данные (`/data` или `/var/lib/mihomyak`, права 0700)

```
machine-id              зерно HWID (не терять!)
secret                  секрет API mihomo
mode                    режим, выбранный через CLI/TUI
supervisor.pid          pid работающего супервизора
subscription/body       последняя принятая подписка (сырое тело)
subscription/meta.json  заголовки, время, ключ кэша, ошибка, следующее обновление
mihomo/                 home mihomo: config.yaml, providers/, cache.db, geo-базы
```

## Тесты

| Уровень | Где | Что проверяет |
|---------|-----|---------------|
| unit | `src/**` (`#[cfg(test)]`) | парсеры, формулы, сборка конфига, cron, фильтры |
| golden | `tests/golden_requests.rs` + `tests/fixtures/requests/` | байты запроса совпадают с перехваченными у настоящих клиентов |
| mihomo | `tests/mihomo_validate.rs` | сгенерированные конфиги проходят `mihomo -t` (нужен `MIHOMYAK_TEST_MIHOMO`) |
| e2e вручную | `dev/mock_panel.py` | мок Remnawave: правила по UA, HWID-лимит, заглушки, Xray JSON |

Сквозной сценарий, проверенный при разработке: мок-панель, второй mihomo как
shadowsocks-сервер, mihomyak (на хосте и в Docker-шлюзе с TUN), трафик клиента
через прокси, отказ при остановке сервера, HWID-лимит, заглушки, fallback-группа,
cron, `on_start`, падение ядра и перезапуск.
