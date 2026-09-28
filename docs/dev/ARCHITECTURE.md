# Архитектура

## Процессы

```
┌─────────────── mihomyak run (PID 1 в контейнере, ~2–4 МБ RSS) ───────────────┐
│ поток signals ──(TERM/INT/HUP/CHLD)──▶ главный цикл (recv_timeout)            │
│                                         │                                      │
│   расписание (interval / cron / on_start) ─▶ Updater: fetch → analyze → build │
│                                         │           → mihomo -t → backup → write│
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
| **`cli/`** | командная строка |
| `cli/mod.rs` | clap-описание команд и справка по группам (`GROUPS`) |
| `cli/commands/` | реализация команд: `subscription.rs` (update, fetch, identity, status, check, render), `proxy.rs` (proxies, select, test, mode), `core_cmd.rs` (core, health) |
| `cli/tui.rs` | ratatui-интерфейс (feature `tui`) |
| **`config/`** | настройки: схема и значения по умолчанию (`mod.rs`), переменные `MIHOMYAK_*` (`env.rs`), проверки (`validate.rs`) |
| **`client/`** | как mihomyak выглядит для панели |
| `client/identity.rs` | machine-id (в том числе из seed), os-release (два парсера: по спецификации и «как регулярка Koala»), hostname, локаль |
| `client/emulation.rs` | **точные** заголовки FlClashX / Koala / Happ, формулы HWID и UA |
| `client/http` | свой HTTP/1.1-клиент: заголовки как есть, rustls без ALPN, chunked, gzip/deflate/br/zstd, CONNECT-прокси; все размеры ограничены, общий дедлайн запроса; IDN → punycode; метка сокета (`SO_MARK`) и свой DNS-клиент (`dns.rs`) для kill switch |
| **`subscription/`** | ответ панели |
| `subscription/mod.rs` | загрузка с редиректами, `analyze` → `Problem` (Refused/Http/Invalid/Stub) |
| `subscription/headers.rs` | `subscription-userinfo`, `profile-*`, `announce`, `x-hwid-*`, `flclashx-newdomain` |
| `subscription/body.rs` | формат тела: YAML / ссылки / base64 / Xray JSON / HTML; адреса узлов |
| `subscription/stub.rs` | распознавание заглушек (0.0.0.0/loopback/port ≤ 1, HWID-отказы) |
| `subscription/xray.rs` | Xray JSON → прокси mihomo (маппинг как в генераторе Remnawave) |
| **`mihomo/`** | всё про ядро |
| `mihomo/profile.rs` | сборка `config.yaml`: белый список ключей подписки, безопасные типы прокси и пути провайдеров, фильтр со всеми ссылками, группы, правила, управляемые ключи, TUN, `[mihomo]` |
| `mihomo/core.rs` | процесс mihomo (очищенное окружение, `PR_SET_PDEATHSIG`, SIGTERM → SIGKILL), поиск бинарника, `core install`, geo-базы |
| `mihomo/api.rs` | REST API mihomo (proxies, select, delay, configs, connections); `Rejected` отличает отказ mihomo от недоступности |
| **`service/`** | фоновая работа |
| `service/supervisor.rs` | цикл `run`: сигналы, запуск и перезапуск mihomo, hot reload, группы по умолчанию, reaping, быстрые повторы до первого конфига |
| `service/updater.rs` | конвейер обновления: проверка `mihomo -t`, резервные копии `*.prev` и откат, кэш, расписание |
| `service/schedule.rs` | cron (5 полей) и локальное время через `localtime_r` |
| `service/store.rs` | каталог данных: machine-id, secret, блокировка супервизора (flock), mode, кэш подписки, метаданные |
| **`gateway/`** | режим шлюза: проверка встроенного DNS Docker, который пересылает запросы мимо TUN (`mod.rs`), и kill switch — цепочка iptables/ip6tables, выпускающая только TUN, помеченный трафик, ответы и частные сети (`killswitch.rs`); DNS-запросы самого mihomyak при этом помечены (`client/http/dns.rs`) |
| **`util/`** | время без chrono, размеры, sha256 (ring), атомарная запись (`mod.rs`), логгер (`log.rs`), glob-маски имён узлов и их перевод в Go-regex (`pattern.rs`) |

## Ключевые решения

1. **Свой HTTP-клиент.** Главное требование — побайтная имитация клиентов. Все
   распространённые клиенты (reqwest, ureq, hyper) проводят заголовки через
   крейт `http`: имена приводятся к нижнему регистру, порядок и неявные заголовки
   задаёт библиотека. У Koala заголовки в смешанном регистре, у FlClashX — свой
   порядок из HashMap Dart. Нужное подмножество HTTP/1.1 небольшое (около 1000
   строк вместе с тестами).
2. **mihomo — единственное ядро.** Ссылки разбирает сам mihomo (file-провайдер).
   Xray JSON (ответ панели для Happ) конвертируется по той же таблице полей, что
   использует генератор mihomo в Remnawave. Всё, что не переносится, попадает в лог.
3. **Кэш — источник правды при старте.** Как реальные клиенты, mihomyak не
   дёргает панель при каждом рестарте: конфиг собирается из последней принятой
   подписки, а обновление идёт по расписанию (или сразу, если `on_start`). Сеть
   поднимается мгновенно даже при недоступной панели.
4. **Заглушка никогда не заменяет рабочий конфиг.** `analyze` классифицирует
   ответ; применяется только `usable()`, и только после `mihomo -t`. Если живое ядро
   отвергло горячую перезагрузку (ответ 4xx), файлы `*.prev` возвращаются на место;
   если API недоступен, ядро перезапускается.
5. **Подписка — недоверенный вход.** Из неё берётся белый список ключей; порты,
   listeners, туннели, TUN, контроллер, пути файлов провайдеров и режим решает
   клиент. Панель тоже не доверенная сторона для HTTP-клиента: ограничены строки,
   заголовки, chunked-блоки, распакованный размер, время всего запроса.
6. **Один писатель.** Подписку пишет только супервизор; `mihomyak update` шлёт
   ему SIGHUP и ждёт увеличения `update_seq` в метаданных. Если супервизор не
   запущен, команда пишет сама. Супервизор держит `flock` на `supervisor.pid`: второй
   экземпляр не стартует, устаревший pid-файл не мешает, а чужой процесс с тем же
   pid никогда не получит сигнал.
7. **Безопасность по умолчанию.** loopback-прокси, `lan-allowed-ips`, секрет API,
   umask 077, файлы 0600, hardening в compose. Подробности — в README и
   [DOCKER.md](../DOCKER.md).
8. **Минимум зависимостей.** Никаких chrono, regex, tokio, reqwest. Криптография —
   ring (уже нужен rustls); zstd и brotli — на чистом Rust.

## Данные (`/data` или `/var/lib/mihomyak`, права 0700)

```
machine-id              зерно HWID (не терять!)
secret                  секрет API mihomo
mode                    режим, выбранный через CLI/TUI
supervisor.pid          pid работающего супервизора (flock, пока он жив)
subscription/body       последняя принятая подписка (сырое тело) (+ body.prev)
subscription/meta.json  заголовки, время, ключ кэша, ошибка, следующее обновление,
                        update_seq, хосты и адреса панели
mihomo/                 home mihomo: config.yaml (+ .prev), providers/, cache.db, geo-базы
```

## Тесты

| Уровень | Где | Что проверяет |
|---------|-----|---------------|
| unit | `src/**` (`#[cfg(test)]`) | парсеры, формулы HWID и заголовков, сборка конфига, cron, фильтры, права файлов, настройки |
| golden | `tests/golden_requests.rs` + `tests/fixtures/requests/` | байты запроса совпадают с перехваченными у настоящих клиентов |
| интеграционные | `tests/updater_pipeline.rs`, `tests/api_client.rs` | конвейер обновления и клиент API mihomo против встроенных фейковых серверов |
| mihomo | `tests/mihomo_validate.rs` | сгенерированные конфиги проходят `mihomo -t` (нужен `MIHOMYAK_TEST_MIHOMO`) |
| Docker smoke | `tests/e2e/smoke.sh` | образ стартует в hardened-режиме, шлюз без `dns:` не стартует |
| шлюз e2e | `tests/e2e/gateway.sh` + `gateway.compose.yml` | стенд с мок-панелью, двумя узлами и «сайтом» за ними, на amd64 и arm64 |

Стенд `gateway.sh` проверяет: трафик клиента идёт через узел (цель видит адрес узла),
DNS отдаёт fake-ip, явный прокси для соседнего контейнера, `update` через SIGHUP,
ошибки и заглушки панели не заменяют рабочий конфиг, лимит устройств, форматы Happ,
враждебную панель, переключение fallback-группы и возврат, kill switch при убитом
mihomo и замороженном супервизоре, старт из кэша после перезапуска с переподключением
клиента, чистое снятие правил по SIGTERM, RSS супервизора. Тот же сценарий
проверялся вручную на Raspberry Pi 3B+.
