# Разработка

Как устроен код — в [ARCHITECTURE.md](ARCHITECTURE.md), как ведут себя панели и
клиенты, которых мы имитируем, — в [SUBSCRIPTIONS.md](SUBSCRIPTIONS.md).

## Сборка и проверки

Нужны Rust (stable, MSRV 1.88), для статических сборок — clang и lld.

```sh
make check        # fmt, clippy (с TUI и без), тесты, rustdoc — как в CI
make test         # только тесты
make static TARGET=aarch64-unknown-linux-musl     # статический бинарник
make e2e          # собрать образ и прогнать Docker smoke-тест
./tests/e2e/gateway.sh mihomyak:local             # полный стенд шлюза (Docker + /dev/net/tun)
```

Без make — те же команды из `Makefile`: `cargo fmt --all`, `cargo clippy --all-targets
-- -D warnings`, `cargo test`. `MIHOMYAK_TEST_MIHOMO=/path/to/mihomo cargo test`
дополнительно проверяет сгенерированные конфиги настоящим `mihomo -t`.

## Тесты

| Где | Что |
|-----|-----|
| `src/**` | unit-тесты рядом с кодом |
| `tests/golden_requests.rs` | запрос подписки побайтно совпадает с перехваченным у настоящих клиентов |
| `tests/updater_pipeline.rs`, `tests/api_client.rs` | конвейер обновления и клиент API mihomo против встроенных фейковых серверов |
| `tests/mihomo_validate.rs` | конфиги проходят `mihomo -t` |
| `tests/e2e/smoke.sh` | образ: hardened-контейнеры, прокси, шлюз, отказ без `dns:` |
| `tests/e2e/gateway.sh` | стенд шлюза: мок-панель, два ss-узла, «сайт» за ними; сценарии — в [ARCHITECTURE.md](ARCHITECTURE.md#тесты) |

Стенд `gateway.sh` создаёт только ресурсы `mhk-*` и удаляет их за собой, поэтому его
можно запускать и на своём сервере (например, Raspberry Pi) с готовым образом.

### Golden-фикстуры

`tests/fixtures/requests/*.http` — сырые заголовки запросов, перехваченные у
настоящих клиентов на Ubuntu 24.04 с `/etc/machine-id = 0d0af05ee8fd4dc29275718f2ce4dff1`.
`{PORT}` — порт тестового сервера, окончания строк CRLF значимы (`.gitattributes`).

| Файл | Откуда |
|------|--------|
| `flclashx-0.4.2-linux.http` | FlClashX v0.4.2 `.deb`, headless под Xvfb, автообновление профиля |
| `koala-1.4.1-linux.http` | путь `createProfile()` Koala Clash 1.4.1 на её собственном axios 1.15.1 в Node из Electron 37; `x-hwid` — значение `deviceInfo.ts` для machine-id выше |
| `happ-4.3.0-linux-x64.http` | Happ Desktop 4.3.0 x64 `.deb`, headless под Xvfb через `happ://add/…`, hostname `rpi-box`, machine-id `11112222333344445555666677778888`; цифра-маркер в UA зависит от дня (`emulation::happ_day_marker`) |

### Новая версия эмулируемого клиента

1. Проверьте исходники (FlClashX `lib/common/package.dart`,
   `lib/utils/device_info_service.dart`; Koala `src/main/utils/userAgent.ts`,
   `deviceInfo.ts`, `config/profile.ts`) или, для Happ, перехватите запрос новой сборки
   ([SUBSCRIPTIONS.md](SUBSCRIPTIONS.md) §7).
2. Обновите константы в `src/client/emulation.rs`, фикстуру и §7 SUBSCRIPTIONS.md.
   Пользователи могут поменять версии без пересборки: `subscription.app_version`,
   `app_build`, `core_version`.

## Правила кода

- Rust 2024, `rustfmt.toml`, clippy без предупреждений (`-D warnings`).
- Без тяжёлых зависимостей (tokio, reqwest, regex, chrono): бинарник ~3 МБ, супервизор
  в простое 2–3 МБ RSS.
- Каждая константа имитации подтверждена исходником или перехваченным запросом и
  описана в SUBSCRIPTIONS.md; непроверенное помечено.
- Секреты не попадают в логи: `subscription::redact` для ссылок, ошибки
  `client::http::Url` не содержат вход.
- Всё от панели — недоверенный вход: новые ключи подписки проходят белый список в
  `mihomo/profile.rs` (`PROVIDER_KEYS`, `PROVIDER_DNS_KEYS`, `PROXY_TYPES`), текст
  провайдера — через `util::sanitize`, HTTP-клиент ограничивает каждое чтение.
- Конфиг попадает к mihomo только через `Updater` (сборка → `mihomo -t` → резервная
  копия → запись).
- Новые команды CLI добавляются в `GROUPS` в `src/cli/mod.rs` (это проверяет тест).
- Коммиты в стиле Conventional Commits (`feat:`, `fix:`, `docs:`…).

## CI

`.github/workflows/ci.yml` на каждый PR и push в `main`:

- fmt, clippy (с TUI и без), тесты с настоящим `mihomo -t`, rustdoc, MSRV;
- статические сборки x86_64, aarch64, armv7; для ARM тесты гоняются под qemu;
- аудит зависимостей (`cargo deny`);
- образ из готовых бинарников (`Dockerfile`, `BINARY=prebuilt`) и Docker smoke-тест;
  arm64-образ выкладывается артефактом `image-arm64`;
- стенд шлюза на amd64 и нативно на arm64 (на PR — если менялся код, стенд, образ
  или CI);
- покрытие тестами в сводке прогона (информационно);
- сборка образа из исходников — на `main` и в PR, меняющих Dockerfile или зависимости.

Push в `main` публикует мультиарх-образ `:edge`.

## Релиз

1. Перенесите заметки из `## [Unreleased]` в `## [X.Y.Z] - дата` в `CHANGELOG.md` и
   поднимите `version` в `Cargo.toml`.
2. Влейте в `main`.
3. Actions → Release → Run workflow (тег `vX.Y.Z` создастся сам) или push тега.

Релиз переиспользует CI и публикует ровно проверенные бинарники: архивы
`mihomyak-<target>.tar.gz` с `SHA256SUMS`, заметки из CHANGELOG и образ
`:X.Y.Z`/`:X.Y`/`:latest`. После публикации образ скачивается на amd64 и arm64 и
проходит smoke-проверку.
