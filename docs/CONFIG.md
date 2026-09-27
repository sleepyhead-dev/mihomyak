# Настройка

Источники настроек (каждый следующий перекрывает предыдущий):

1. встроенные значения по умолчанию;
2. TOML-файл: `--config`, иначе `$MIHOMYAK_CONFIG`, иначе `/etc/mihomyak/config.toml`
   (root) или `$XDG_CONFIG_HOME/mihomyak/config.toml` (обычно
   `~/.config/mihomyak/config.toml`); в Docker — `/data/config.toml`. Если файла по
   `--config` нет, это ошибка; если нет файла по `$MIHOMYAK_CONFIG`, ищется файл по
   умолчанию, а без него работают значения по умолчанию и переменные окружения (так в
   Docker конфиг необязателен);
3. переменные окружения `MIHOMYAK_*`.

Полный пример с комментариями: [examples/config.toml](../examples/config.toml).
Проверка настроек: `mihomyak check`. Итоговый конфиг mihomo: `mihomyak render`.

Неизвестный ключ в TOML — это ошибка, а не молчаливое игнорирование. Настройки
читаются при запуске: после изменения конфига или переменных перезапустите
супервизор (`systemctl restart mihomyak`, `docker compose restart`). Файл с ссылкой
на подписку держите с правами `600`, иначе в лог пишется предупреждение.

## `[subscription]`

| Ключ | По умолчанию | Env | Описание |
|------|--------------|-----|----------|
| `url` | — | `MIHOMYAK_SUB_URL` | ссылка на подписку (секрет: в логах маскируется) |
| `client` | `flclashx` | `MIHOMYAK_CLIENT` | `flclashx`, `koala`, `happ`, `custom` |
| `app_version` | версия клиента | `MIHOMYAK_APP_VERSION` | версия эмулируемого клиента |
| `app_build` | build id Happ | — | Happ: `…/Linux/<build>…` |
| `core_version` | `v1.19.28` | — | FlClashX: `core/<ver>` в UA и `global-ua` |
| `user_agent` | по клиенту | `MIHOMYAK_USER_AGENT` | заменить только значение UA; для `custom` обязателен |
| `headers` | `[]` | — | `["Name: value"]`: добавить или заменить заголовки (имя — токен HTTP; у FlClashX порядок пересчитывается, как в dart:io) |
| `proxy` | — | `MIHOMYAK_FETCH_PROXY` | качать подписку через `http://host:port` (CONNECT) |
| `accept_stub` | `false` | `MIHOMYAK_ACCEPT_STUB` | применять конфиги-заглушки |

Значения, которые уходят в заголовки запроса (`user_agent`, `headers`, версии,
`[device]`), не могут содержать управляющие символы: перевод строки позволил бы
дописать в запрос чужие заголовки. Ссылка по `http://` работает, но в лог пишется
предупреждение: токен и HWID идут открытым текстом. Редирект с https на http
не выполняется.

## `[update]`

| Ключ | По умолчанию | Env | Описание |
|------|--------------|-----|----------|
| `interval` | `auto` | `MIHOMYAK_UPDATE_INTERVAL` | `auto` (`profile-update-interval` провайдера, иначе 24h), `off`, `6h`, `30m`… (минимум 5 минут) |
| `cron` | `[]` | `MIHOMYAK_UPDATE_CRON` (`;`-список) | cron из 5 полей в локальном времени (`TZ`): `0 5 * * *`, `*/30 * * * *`, `@daily`, имена `mon-fri`, `jan` |
| `on_start` | `false` | `MIHOMYAK_UPDATE_ON_START` | обновлять при каждом старте; пока идёт загрузка, работает кэш |

Следующее обновление — самое раннее из интервала и всех cron-выражений. Если
запуск был пропущен (машина была выключена в 05:00), он выполняется сразу при
старте. При ошибке следуют повторы через 1, 2, 4… минут, максимум через 1 час;
рабочий конфиг при этом не трогается. При самом первом запуске, пока кэша ещё нет
и mihomo не работает, сетевые ошибки (не готов DNS, нет связи) повторяются
быстрее: через 5, 10, 20, 40 с и дальше так же с удвоением. `profile-update-interval` провайдера тоже не
может быть меньше 5 минут.

cron работает как Vixie cron: если заданы и день месяца, и день недели, достаточно
совпадения любого из них, но поле, начинающееся с `*` (например, `*/2`), считается
«любым». Выражения, которые никогда не срабатывают (`0 0 31 2 *`), отклоняются.
При старте в лог пишется смещение локального времени (`UTC+03:00`): если там
`+00:00`, значит `TZ` не задан.

Каждый новый конфиг перед применением проверяется `mihomo -t`. Если mihomo его
отвергает, подписка не применяется (ошибка видна в `mihomyak status`). Если
работающий mihomo всё же отверг конфиг при горячей перезагрузке, возвращаются
предыдущие файлы (`config.yaml.prev` и др.), и ядро продолжает работать на старом
конфиге.

## `[filter]`

| Ключ | Env | Описание |
|------|-----|----------|
| `include` | `MIHOMYAK_INCLUDE` (`;`-список) | оставить только узлы, подходящие под одну из масок (пусто — все) |
| `exclude` | `MIHOMYAK_EXCLUDE` (`;`-список) | выбросить подходящие узлы |

Маски — glob без учёта регистра: `*` — любые символы, `?` — один символ
(`*🇳🇱*`, `*Германия*`, `NL-?`). Фильтр применяется при каждом обновлении:
- узлы удаляются из `proxies` и из всех групп; если группа опустела, в неё
  ставится `DIRECT`, и в лог пишется предупреждение;
- для подписок-ссылок выставляются `filter`/`exclude-filter` провайдера mihomo.

## `[[groups]]` — автопереключение

| Ключ | По умолчанию | Описание |
|------|--------------|----------|
| `name` | — | уникальное имя; зарезервированы `DIRECT`, `REJECT`, `GLOBAL`, `PROXY`, `AUTO` |
| `type` | `fallback` | `fallback` (первый живой по приоритету), `url-test` (самый быстрый), `load-balance`, `select` |
| `nodes` | `[]` (все) | маски в порядке приоритета |
| `url` | `https://www.gstatic.com/generate_204` | проверка живости |
| `interval` | `3m` | период проверки |
| `tolerance` | `50` | url-test: переключаться, только если новый узел быстрее на N мс |
| `default` | `false` | группа добавляется первой в селекторы подписки и выбирается в них при каждом старте ядра и после обновления подписки (ручной выбор держится до этого момента) |

Если в подписке нет групп (ссылки, Xray JSON, голый `proxies:`), mihomyak создаёт
`PROXY` (select) и `AUTO` (url-test) и правила «LAN напрямую, остальное через PROXY».

## `[rules]`

| Ключ | Env | Описание |
|------|-----|----------|
| `prepend` | — | свои правила mihomo перед правилами подписки: `"DOMAIN-SUFFIX,lan,DIRECT"` |
| `presets` | `MIHOMYAK_RULES_PRESETS` | `ru-direct`: `.ru`, `.su`, `.рф`, `geosite:category-ru` и `geoip:ru` идут напрямую |

`ru-direct` использует `GEOIP,ru,DIRECT,no-resolve`: домены не резолвятся ради
проверки страны, чтобы все DNS-запросы не утекали к провайдеру.

## `[device]` — эмулируемое устройство

| Ключ | По умолчанию | Env | Описание |
|------|--------------|-----|----------|
| `machine_id` | генерируется в `<data>/machine-id` | `MIHOMYAK_MACHINE_ID` | зерно HWID (как `/etc/machine-id`) |
| `hwid` | по формуле клиента | `MIHOMYAK_HWID` | итоговый `x-hwid` как есть |
| `send_headers` | `true` | — | слать `x-hwid`/`x-device-*` |
| `os_release` | `/etc/os-release` | `MIHOMYAK_OS_RELEASE` | откуда брать дистрибутив |
| `os_name`, `os_version`, `os_pretty_name` | из os-release | — | переопределить `NAME`/`VERSION_ID`/`PRETTY_NAME` |
| `hostname` | hostname ядра | `MIHOMYAK_HOSTNAME` | Happ: `X-Device-Model: <hostname>_<arch>` |
| `locale` | `en` | `MIHOMYAK_LOCALE` | Happ: `X-Device-Locale`, `Accept-Language` |

Формулы HWID: FlClashX — `sha256(machine-id)[:16]` в верхнем регистре, Koala —
то же в нижнем, Happ — сырой machine-id. HWID проверяется регуляркой Remnawave
`^[a-zA-Z0-9=-]{10,64}$`.

## `[core]` — mihomo

| Ключ | По умолчанию | Env | Описание |
|------|--------------|-----|----------|
| `bin` | `mihomo` | `MIHOMYAK_CORE_BIN` | путь или имя в PATH (иначе `<data>/bin/mihomo`) |
| `controller` | `127.0.0.1:9090` | `MIHOMYAK_CONTROLLER` | API mihomo: `host:port` или `unix:/path` |
| `secret` | генерируется в `<data>/secret` | `MIHOMYAK_SECRET` | секрет API; пустой запрещён, если API не на loopback |
| `mixed_port` | `7890` | `MIHOMYAK_MIXED_PORT` | HTTP+SOCKS5 (0 — выключить) |
| `allow_lan` | `false` | `MIHOMYAK_ALLOW_LAN` | принимать подключения не с loopback |
| `lan_allowed_ips` | частные сети | — | откуда разрешены подключения при `allow_lan`. В Docker под `172.16.0.0/12` попадают **все** контейнеры всех docker-сетей хоста: сузьте до своей сети или задайте `auth` |
| `auth` | `[]` | `MIHOMYAK_PROXY_AUTH` (`;`-список) | `user:password`; loopback без пароля |
| `bind_address` | `*` | — | адрес прослушивания при `allow_lan` |
| `log_level` | `warning` | `MIHOMYAK_LOG_LEVEL` | уровень логов mihomo |
| `mode` | `rule` | `MIHOMYAK_MODE` | режим по умолчанию; `mihomyak mode` сохраняет выбор в `<data>/mode` |
| `memory_limit` | — | `MIHOMYAK_MEMORY_LIMIT` | `GOMEMLIMIT` для mihomo (`64MiB`) |
| `geodata_dir` | — (в Docker задан) | `MIHOMYAK_GEODATA_DIR` | откуда копировать geo-базы, которых ещё нет в каталоге mihomo (при каждом старте, существующие не перезаписываются) |

## `[gateway]` — прозрачный шлюз (TUN)

| Ключ | По умолчанию | Env | Описание |
|------|--------------|-----|----------|
| `enable` | `false` | `MIHOMYAK_GATEWAY` | TUN + auto-route + перехват DNS |
| `stack` | `system` | — | `system` (легче всего), `gvisor`, `mixed` |
| `auto_redirect` | `false` | — | nftables-redirect для TCP (быстрее, нужен nf_tables) |
| `dns_listen` | `127.0.0.1:1053` | — | DNS mihomo; TUN перехватывает :53 в любом случае |
| `kill_switch` | `false` | `MIHOMYAK_KILL_SWITCH` | блокировать трафик мимо mihomo, пока тот не работает (см. ниже) |

Хосты панели (адрес подписки и редиректы) и их IP-адреса выводятся из-под туннеля:
домены попадают в `fake-ip-filter` (с учётом `fake-ip-filter-mode`), адреса — в
`route-exclude-address`. Так подписка обновляется, даже если все узлы мертвы.
Обратная сторона: трафик приложений к этим адресам тоже идёт напрямую.

### Kill switch

Пока mihomo не запущен (старт, перезапуск после падения), TUN нет, и без kill
switch трафик контейнеров идёт напрямую. `kill_switch = true` ставит при старте
цепочку `MIHOMYAK` в `OUTPUT` (iptables и ip6tables, атомарно через
`iptables-restore`) и выпускает только:

- трафик в TUN (`mihomyak0`) и через loopback;
- соединения самого mihomo (`routing-mark`) и mihomyak (метка сокета), чтобы узлы
  и панель подписки оставались доступны;
- ответы на входящие соединения (опубликованные порты работают);
- DNS (порт 53): без него Docker-резолвер и сам супервизор не найдут панель. Имена,
  которые запрашивают приложения, в это время видны DNS-серверу, сами данные — нет;
- частные адреса (`10/8`, `172.16/12`, `192.168/16`, `169.254/16`, `fc00::/7`,
  `fe80::/10`): соседние контейнеры и LAN.

Остальное отклоняется сразу (приложение получает «connection refused», а не
зависает). Трафик приложений к адресу панели тоже блокируется: он идёт мимо
туннеля. Нужны `iptables` (есть в Docker-образе) и `CAP_NET_ADMIN` (у шлюза уже
есть); без них супервизор не стартует, а не работает молча без защиты. Правила
снимаются при штатной остановке и остаются после аварийной. На голом железе kill
switch действует на весь хост (кроме пересылаемого LAN-трафика).

## `[mihomo]` — любые ключи mihomo

Произвольные ключи конфигурации mihomo, которые накладываются последними
(deep-merge, массивы заменяются целиком). Исключение — `external-controller` и
`secret`: их всегда задаёт `[core]`, иначе CLI не сможет управлять ядром. В отличие
от подписки, `[mihomo]` — ваш собственный файл, и ограничений на ключи здесь нет.

```toml
[mihomo]
ipv6 = false
unified-delay = true
dns = { nameserver = ["https://1.1.1.1/dns-query"] }
```

## Прочие переменные

| Env | Описание |
|-----|----------|
| `MIHOMYAK_DATA_DIR` | каталог данных (по умолчанию `/var/lib/mihomyak` для root, иначе `~/.local/share/mihomyak`; в Docker `/data`) |
| `MIHOMYAK_LOG` | уровень логов mihomyak: `error`, `warn`, `info` (по умолчанию), `debug` |
| `MIHOMYAK_GITHUB_MIRROR` | зеркало вместо `https://github.com` для `core install`; с зеркалом обязателен `--sha256` |
| `TZ` | часовой пояс для cron (время в логах всегда UTC) |
| `SSL_CERT_FILE` | дополнительные доверенные CA (PEM) для загрузки подписки |
| `MIHOMYAK_TEST_MIHOMO` | путь к mihomo для тестов `tests/mihomo_validate.rs` |

## Что mihomyak делает с конфигом провайдера

Подписка — недоверенный вход: провайдер не должен решать, что открыто на вашем
сервере. Порядок сборки `config.yaml` (`src/profile.rs`):

1. берётся YAML подписки (для ссылок — каркас с file-провайдером, для Xray JSON —
   сконвертированные прокси);
2. из него остаётся только **белый список** ключей: `proxies`, `proxy-groups`,
   `proxy-providers`, `rule-providers`, `rules`, `sub-rules`, политика `dns` (без
   `listen`), `sniffer` и несколько сетевых настроек (`ipv6`, `unified-delay`,
   `tcp-concurrent`, `keep-alive-*`, `global-client-fingerprint`, `global-ua`,
   `geodata-*`). Всё остальное (порты, `listeners`, `tunnels`, `tun`, контроллер,
   `authentication`, `geox-url`, `hosts`, …) отбрасывается, о неожиданных ключах
   пишется предупреждение;
3. прокси небезопасных типов (оверлейные сети `tailscale`, `zerotier`, `easytier` и
   всё неизвестное) удаляются, в провайдерах они исключаются через `exclude-type`;
   `http`-провайдеры получают путь по умолчанию (провайдер не может перезаписать
   `config.yaml` или `cache.db`), `file`-провайдеры удаляются;
4. `[filter]`; все ссылки на удалённые узлы исправляются: группы без узлов
   получают `DIRECT`, правила на них уходят в `DIRECT`, `dialer-proxy` снимается;
5. автогруппы (если групп нет) → `[[groups]]` → `[rules]`;
6. управляемые ключи: порт, LAN, `mode` (клиент владеет режимом, как FlClashX:
   `mode: global` из шаблона Remnawave игнорируется), `profile.store-selected`,
   `find-process-mode: off`, `global-ua` для FlClashX;
7. `[gateway]` (TUN, DNS, обход туннеля для панели);
8. `[mihomo]`;
9. контроллер и секрет;
10. проверка `mihomo -t` перед заменой рабочего конфига.
