# Как устроены «СНГ-подписки» и что нужно для их стабильного получения

Исследование от 2026-09-25. Всё ниже проверено одним из трёх способов (помечено):

- **[src]** прочитано в исходном коде (панели или клиента), с путём к файлу;
- **[cap]** перехвачено: реальный клиент запускался в песочнице и слал запрос на
  локальный TCP-сервер, который записывал сырые байты;
- **[re]** найдено в бинарнике (strings + дизассемблирование), для закрытых клиентов.

Всё, что не проверено, помечено словом **«не проверено»**.

---

## 1. Общая схема

```
клиент ──GET https://sub.example.com/<shortUuid>──▶ панель
        User-Agent: <какой клиент>             (Remnawave / Marzban / PasarGuard / 3x-ui)
        x-hwid: <id устройства>
        x-device-os / x-ver-os / x-device-model
                                          ◀── тело: конфиг в формате, выбранном по UA
                                              заголовки: subscription-userinfo, profile-title, …
```

Ссылка на подписку — это секретный идентификатор пользователя (`shortUuid` / token).
Панель по заголовкам запроса решает:

1. **какой формат отдать** — по `User-Agent` (и `Accept`);
2. **пускать ли устройство** — по `x-hwid` (лимит устройств, «HWID device limit»);
3. **что показать вместо конфига**, если пользователь истёк, превысил трафик,
   устройство не поддерживается или лимит устройств исчерпан — это и есть «заглушка».

Отсюда вывод: для стабильной работы клиент должен

- слать `User-Agent`, который панель относит к mihomo-клиентам (иначе придёт base64 / JSON / HTML);
- слать стабильный `x-hwid` в допустимом формате (иначе 404/пустое тело/заглушка);
- не менять HWID между обновлениями (иначе каждое изменение занимает новый слот устройства);
- распознавать заглушку и **не** заменять ею последний рабочий конфиг.

---

## 2. Remnawave (самая популярная панель в СНГ)

Исходники: `github.com/remnawave/backend` (NestJS).

### 2.1 Выбор формата: Subscription Response Rules (SRR)  [src]

Файл: `prisma/seed/default/response-rules.ts` — правила по умолчанию, проверяются сверху
вниз, срабатывает первое совпавшее:

| # | Условие | Ответ |
|---|---------|-------|
| 1 | `accept` содержит `text/html` (регистр важен) | `BROWSER` — HTML-страница подписки |
| 2 | `user-agent` ~ `/^(?:flclash\|rabbit\|flowvy\|murge\|mihomo\|prizrak-box\|koala-clash\|clash(?:-verge\|-nyanpasu\|x meta\|[-.]?meta))/i` | `MIHOMO` — YAML по шаблону Mihomo |
| 3 | `user-agent` ~ `/^stash/i` | `STASH` |
| 4 | `user-agent` ~ `/^sfa\|sfi\|sfm\|sft\|karing\|singbox/i` | `SINGBOX` (JSON) |
| 5 | `user-agent` ~ `/^clash/i` | `CLASH` (legacy YAML) |
| 6 | без условий | `XRAY_BASE64` — base64 со ссылками `vless://…` |

Админ может переписать правила: например отдавать `BLOCK` (403), `STATUS_CODE_404`,
`STATUS_CODE_451`, `SOCKET_DROP` (обрыв соединения) неизвестным клиентам. Это и есть
«белый список клиентов» у многих провайдеров.

- Пустой `User-Agent` → **403** ещё до матчинга
  (`src/modules/subscription-response-rules/middleware/response-rules.middleware.ts`).
- Нет совпавшего правила → 403.

**Вывод для нас:** UA `FlClash X/…` и `koala-clash/…` попадают в правило 2 и получают
готовый YAML для mihomo.

### 2.2 «Расширенные клиенты»  [src]

`src/modules/subscription-template/constants/extended-clients.ts`:

```ts
EXTENDED_CLIENTS_REGEXES = [/^FlClash ?X\//, /^Flowvy\//, /^prizrak-box\//,
                            /^koala-clash\//, /^Happ\//, /^INCY\//]
```

Им в шаблон добавляются доп. поля (например описание сервера `serverDescription`).
UA FlClashX (`FlClash X/v…`) и Koala (`koala-clash/…`) распознаются как расширенные.

### 2.3 HWID device limit  [src]

`src/common/utils/extract-hwid-headers/extract-hwid-headers.util.ts`:

```ts
const HWID_REGEX = /^[a-zA-Z0-9=-]{10,64}$/;   // с v3.0.0
hwid       = headers['x-hwid']          // обязателен
platform   = headers['x-device-os']     // опционально
osVersion  = headers['x-ver-os']
deviceModel= headers['x-device-model']
userAgent  = headers['user-agent']
```

HWID, не прошедший регулярку, **считается отсутствующим**. Отсюда требование:
10–64 символа, только `[a-zA-Z0-9=-]`.

Логика (`src/modules/subscription/subscription.service.ts`, `checkHwidDeviceLimit`):

| Ситуация | Результат |
|----------|-----------|
| HWID-лимит выключен глобально | устройство всё равно записывается (если x-hwid есть), конфиг отдаётся |
| у пользователя `hwidDeviceLimit = 0` | лимит обходится, конфиг отдаётся |
| нет валидного `x-hwid` | `subscriptionAllowed=false, hwidNotSupported=true` |
| HWID уже известен | конфиг отдаётся |
| новый HWID и слот есть | устройство регистрируется, конфиг отдаётся |
| новый HWID и слотов нет | `maxDeviceReached=true` |

При отказе ответ такой:

- статус **200**, `content-type: text/plain`, тело **пустое** — либо, если в настройках
  включено `isShowCustomRemarks`, тело — конфиг-заглушка (см. 2.5);
- заголовки: `x-hwid-active: true`, `x-hwid-not-supported: true` **или**
  `x-hwid-max-devices-reached: true`, всегда `x-hwid-limit: true` (для v2RayTun);
- при исчерпании лимита может прийти `announce: base64:<текст>` (`maxDevicesAnnounce`);
- обычные `subscription-userinfo` и `content-disposition` тоже присутствуют.

При успехе, если лимит активен, добавляется `x-hwid-active: true`.

### 2.4 Заголовки ответа  [src]

`getUserProfileHeadersInfo()` + `customResponseHeaders` из настроек
(seed: `prisma/seed/seeders/8_seed-subscription-settings.ts`):

| Заголовок | Формат | Пример |
|-----------|--------|--------|
| `subscription-userinfo` | `upload=N; download=N; total=N; expire=UNIX` (байты; `expire=0` — бессрочно, `total=0` — безлимит) | `upload=0; download=123; total=107374182400; expire=1767225600` |
| `content-disposition` | `attachment; filename=<username>` | |
| `subscription-refill-date` | unix-время следующего сброса трафика | |
| `profile-title` | строка или `base64:<…>` | `base64:UmVtbmF3YXZl` |
| `profile-update-interval` | часы (целое) | `12` |
| `support-url`, `profile-web-page-url` | URL | |
| `announce` | строка или `base64:<…>` | |
| `routing` | Happ-routing (для Happ) | |
| `x-hwid-*` | см. выше | |

Remnawave всегда шлёт `upload=0`, весь трафик — в `download`.

### 2.5 Как выглядит заглушка  [src]

`src/modules/subscription-template/resolve-proxy/resolve-proxy-config.service.ts`,
`createFallbackHosts()`: для каждой строки-«ремарки» создаётся фиктивный сервер

```
protocol: vless, address: 0.0.0.0, port: 1, id: 00000000-0000-0000-0000-000000000000,
security: none, transport: tcp, finalRemark: <текст ремарки>
```

Тексты по умолчанию (seed):

| Случай | Ремарки |
|--------|---------|
| истекла | `⌛ Subscription expired`, `Contact support` |
| трафик исчерпан | `🚧 Subscription limited`, `Contact support` |
| отключена | `🚫 Subscription disabled`, `Contact support` |
| нет хостов | `→ Remnawave`, `→ No hosts found`, … |
| лимит устройств | `Limit of devices reached` |
| клиент без HWID | `App not supported` |

Провайдеры обычно переписывают их по-русски («Обновите приложение», «Лимит устройств» …).
**Надёжный признак заглушки — адрес `0.0.0.0` и порт `1`, а не текст.** В YAML для mihomo
это `server: 0.0.0.0`, `port: 1`.

### 2.6 Прочее

- Шаблон Mihomo по умолчанию (`default-templates.ts`) содержит `mode: global`,
  `external-controller: 127.0.0.1:9090`, `allow-lan: true`, DNS fake-ip. Клиент должен
  переопределять сетевые ключи сам (так делают FlClashX и Koala).
- Каждый запрос логируется: IP, UA, имя сработавшего правила (`updateAndReportSubscriptionRequest`).
- Remnawave прокидывает `x-remnawave-injected-short-uuid` и `x-remnawave-injected-client-type`
  в свои SRR-заголовки: это внутренняя кухня, клиенту не нужно.

---

## 3. Marzban  [src]

`app/routers/subscription.py` (Gozargah/Marzban). HWID нет (есть сторонние форки).
Выбор формата по UA:

```python
r'^([Cc]lash-verge|[Cc]lash[-\.]?[Mm]eta|[Ff][Ll][Cc]lash|[Mm]ihomo)'  -> clash-meta (mihomo YAML)
r'^([Cc]lash|[Ss]tash)'                                              -> clash
r'^(SFA|SFI|SFM|SFT|[Kk]aring|[Hh]iddify[Nn]ext)'                    -> sing-box
r'^(SS|SSR|SSD|SSS|Outline|Shadowsocks|SSconf)'                      -> outline
v2rayN / v2rayNG / Streisand / Happ                                  -> xray JSON (если включено)
иначе                                                                -> base64 ссылки
Accept содержит text/html                                            -> HTML-страница
```

`FlClash X/…` совпадает с `[Ff][Ll][Cc]lash` → mihomo YAML. `koala-clash/…` **не совпадает** →
base64-ссылки (их разбирает mihomo, см. §6). Заголовки ответа те же:
`subscription-userinfo`, `profile-title`, `profile-update-interval`, `support-url`,
`profile-web-page-url`.

## 4. PasarGuard (преемник Marzban)  [src]

`app/db/migrations/versions/9af04c077ede_init_settings.py`, правило по умолчанию:

```
^(?:FlClashX?|Flowvy|[Cc]lash(?:-(?:[Vv]erge|nyanpasu)|X [Mm]eta|-?[Mm]eta)|[Kk]oala-[Cc]lash|[Mm](?:urge|ihomo)|prizrak-box|clash\.meta) -> clash_meta
```

HWID (`app/operation/subscription.py`, `validate_and_register_hwid`):

- читает `x-hwid`, `x-device-os`, `x-ver-os`, `x-device-model`;
- `forced`-политика без `x-hwid` → **403 "HWID header required"**;
- новый HWID сверх лимита → **403 "Device limit reached"**;
- не-forced и без `x-hwid` → пропускает, лимит не применяется.

## 5. 3x-ui  [src]

`internal/sub/*` (MHSanaei/3x-ui). Путь `/sub/<id>` отдаёт base64-ссылки, `/clash/<id>`
(если включено) — YAML. Заголовки `Subscription-Userinfo`, `Profile-Title`,
`Profile-Update-Interval` и т. п. HWID нет.

---

## 6. Форматы тела и как их понимает mihomo

| Что пришло | Признак | Что делаем |
|------------|---------|-----------|
| mihomo/clash YAML | YAML-мэппинг с `proxies` / `proxy-providers` / `proxy-groups` | берём как есть, переопределяем сетевые ключи |
| base64 со ссылками | тело декодируется из base64 в строки `scheme://` | кладём в файл провайдера; mihomo сам конвертирует `vless/vmess/trojan/ss/ssr/hysteria/hysteria2/tuic/socks/http` |
| список ссылок | строки `scheme://` | так же |
| HTML | `content-type: text/html` или `<!doctype`/`<html` | ошибка «панель не признала клиента» |
| JSON (sing-box/xray) | начинается с `{`/`[` | ошибка «неподдерживаемый формат, смените UA» |
| пусто | 0 байт | ошибка; обычно это отказ по HWID (§2.3) |

Проверено на mihomo v1.19.31: file-провайдер принимает сырой список ссылок и base64
(внутри `adapter/provider/parser.go` при ошибке YAML вызывается `convert.ConvertsV2Ray`).

---

## 7. Клиенты, которые мы имитируем (Linux desktop)

Для каждого клиента ниже — **байт-в-байт** запрос, перехваченный в песочнице
(Ubuntu 24.04, `/etc/machine-id = 0d0af05ee8fd4dc29275718f2ce4dff1`, hostname `vm`).

### 7.1 FlClashX v0.4.2 (Flutter/Dart, ядро mihomo v1.19.28)

Исходники: `github.com/pluralplay/FlClashX`.

**[cap]** (dart:io пишет имена заголовков в нижнем регистре; порядок — итерация
внутреннего `HashMap` Dart, см. ниже):

```http
GET /sub/abc HTTP/1.1
user-agent: FlClash X/v0.4.2 core/v1.19.28 Platform/linux
x-device-model: Ubuntu
x-ver-os: 24.04
accept-encoding: gzip
host: 127.0.0.1:18082
x-device-os: Linux
x-hwid: A3B522EAA6F7DD89

```

**[src]** откуда значения:

| Заголовок | Формула | Файл |
|-----------|---------|------|
| UA | `"FlClash X/v$appVersion"` + ` core/$coreVersion` + ` Platform/${Platform.operatingSystem}` | `lib/common/package.dart`, `lib/state.dart` |
| `x-hwid` | `sha256(machineId).hex[0..16].toUpperCase()`; `machineId` = `/etc/machine-id` (device_info_plus) | `lib/utils/device_info_service.dart` |
| `x-device-os` | `Linux` | там же |
| `x-ver-os` | device_info_plus `versionId` = `VERSION_ID` из os-release, иначе `DISTRIB_RELEASE` из `/etc/lsb-release`; пустое значение всё равно шлётся, заголовка нет, только если нет обоих (например, Arch) | там же |
| `x-device-model` | device_info_plus `name` = `NAME` из os-release, иначе `Linux` | там же |

os-release device_info_plus читает по-своему: `/etc/os-release`, иначе
`/usr/lib/os-release`; строка делится по `=` и принимается, только если частей ровно
две; снимаются только двойные кавычки (отдельно в начале и в конце), пробелы не
обрезаются, при повторе ключа побеждает последняя строка. mihomyak повторяет эти
правила (`OsRelease::dip_get`).

**Порядок заголовков.** dart:io хранит заголовки в `HashMap<String, List<String>>`
(VM-реализация `_HashMap`, `sdk/lib/_internal/vm/lib/collection_patch.dart`) и пишет
их в порядке итерации. Модель: 8 корзин, новый элемент добавляется в начало цепочки,
при `4·n > 3·ёмкость` таблица удваивается (цепочки перекладываются по порядку
корзин), итерация идёт по корзинам от 0; хеш строки — Jenkins one-at-a-time по
UTF-16, обрезанный до 30 бит (0 → 1). Порядок вставки: `host`, `accept-encoding`
(HttpClient), затем `user-agent`, `x-hwid`, `x-device-os`, `x-device-model`,
`x-ver-os` (FlClashX), затем пользовательские заголовки. Модель воспроизводит
перехваченный порядок для 7 ключей; без `x-ver-os` (6 ключей, таблица не растёт)
порядок другой: `user-agent, x-device-model, accept-encoding, x-hwid, x-device-os,
host`. mihomyak вычисляет порядок моделью для любого набора ключей
(`emulation::dart_hashmap_order`).

Проверка: `sha256("0d0af05ee8fd4dc29275718f2ce4dff1")[:16].upper() = A3B522EAA6F7DD89` ✔.

Поведение:
- таймауты: connect 15 s, receive 60 s;
- первый запрос без авторедиректа: при 3xx повторяет запрос на `Location` с теми же
  заголовками (до 5 редиректов);
- интервал обновления: `profile-update-interval` (часы), иначе 24 ч;
- если пришёл заголовок `flclashx-newdomain`, заменяет хост в URL подписки и сохраняет его;
- проверяет тело как mihomo-конфиг (`validateConfig`);
- собирает заголовки `announce`, `support-url`, `profile-update-interval`,
  `x-hwid-max-devices-reached`, `x-hwid-not-supported` и все `flclashx-*`
  (UI-настройки провайдера: `flclashx-servicename`, `-servicelogo`, `-widgets`,
  `-view`, `-settings`, `-globalmode`, `-hex`, `-background`, `-serverinfo`,
  `-custom`, `-denywidgets`, `-buyplan`, `-buytraffic`, `-newboard`, `-androidsecure`).

### 7.2 Koala Clash 1.4.1 (Electron, axios 1.15.1)

Исходники: `github.com/coolcoala/koala-clash`, `src/main/config/profile.ts`,
`src/main/utils/deviceInfo.ts`, `src/main/utils/userAgent.ts`.

**[cap]** (тот же код — axios из `app.asar` клиента, запущенный во встроенном Node 22.21.1):

```http
GET /sub/abc HTTP/1.1
Accept: application/json, text/plain, */*
User-Agent: koala-clash/1.4.1
x-hwid: a3b522eaa6f7dd89
x-device-os: Linux
x-ver-os: Ubuntu 24.04
x-device-model: Ubuntu 24.04.3 LTS
Accept-Encoding: gzip, compress, deflate, br
Host: 127.0.0.1:18081
Connection: keep-alive

```

| Заголовок | Формула |
|-----------|---------|
| UA | `koala-clash/${package.version}` (без `v`) |
| `x-hwid` | `sha256(trim(cat /etc/machine-id \|\| /var/lib/dbus/machine-id)).hex[0..16]` (нижний регистр); если файлов нет — sha256 от MAC-адресов + hostname + модели CPU |
| `x-device-os` | `Linux` |
| `x-ver-os` | `"$NAME $VERSION_ID"` из os-release (регулярка `^NAME="?([^"\n]+)"?`), иначе `NAME`, иначе `uname -r` |
| `x-device-model` | `PRETTY_NAME`, иначе `NAME`, иначе `Linux` |

Особенности:
- тот же machine-id даёт тот же HWID, что и у FlClashX, только в нижнем регистре;
- deep link `clash://install-config?url=…` сначала шлёт `HEAD` со стандартным
  `User-Agent: axios/1.15.1` **без** HWID-заголовков (чтобы прочитать `profile-title`);
- при `x-hwid-limit: true` или `x-hwid-max-devices-reached: true` бросает `HWID_LIMIT`;
- при `content-type: text/html|text/xml` — ошибка формата (так поступает только
  Koala: FlClashX и Happ смотрят на тело, поэтому mihomyak проверяет Content-Type
  только в профиле `koala`);
- дополнительно понимает `profile-web-page-name`, `profile-logo`, `expand-proxy-groups`,
  `profile-update-interval` (часы; блокирует ручное изменение интервала).

### 7.3 Happ Desktop 4.3.0 (Qt 6, ядро xray)

Имитируется профилем `happ`. У Happ xray-ядро, поэтому панель отдаёт ему base64-ссылки
(их понимает mihomo) или Xray JSON (mihomyak конвертирует его в прокси mihomo, §9).
Sing-box в Happ Desktop — только TUN-движок (`tun-type: singbox|tun2proxy|xray|default`),
на формат подписки он не влияет.

**[cap]** (Linux x64, Qt QNetworkAccessManager):

```http
GET /sub/abc HTTP/1.1
Host: 127.0.0.1:18080
User-Agent: Happ/4.3.0/Linux/2609151457698
X-App-Version: 4.3.0
X-Device-Locale: EN
X-Device-Os: Linux
X-Device-Model: vm_x86_64
X-Hwid: 0d0af05ee8fd4dc29275718f2ce4dff1
X-Ver-Os: ubuntu_24.04
Connection: Keep-Alive
Accept-Encoding: zstd, br, gzip, deflate
Accept-Language: en,*

```

**[re]** формулы:

- UA = `QString("Happ/%1/%2/%3%4%5").arg("4.3.0", "Linux", BUILD, C, "98")`, где
  `BUILD` — константа сборки (`2609151457` для x64, `2609151456` для arm64), а
  `C = (QDateTime::currentDateTimeUtc().addSecs(10800).date().day() & 1) ? '5' : '6'`.
  То есть UA **меняется каждый день**: символ зависит от чётности числа месяца по Москве
  (UTC+3). Похоже на примитивную защиту от подделки UA;
- `X-Hwid` = сырой `/etc/machine-id` (32 hex);
- `X-Device-Model` = `<hostname>_<cpu arch>` (`QSysInfo`), `X-Ver-Os` = `<productType>_<productVersion>`;
- `X-Device-Locale` = язык системы в верхнем регистре; `Accept-Language` строит Qt:
  `QLocale::system().name()` через `-`, затем `,*` для английского и `,en,*` для
  остальных; локаль `C` считается `en`. Проверено: `C` → `en,*`, `ru_RU` →
  `ru-RU,en,*`. Для языка без региона (`ru`) Qt подставляет вероятный регион по CLDR
  (`ru-RU`); mihomyak делает так же для распространённых языков, **не проверено
  захватом**;
- в бинарнике есть поддержка `happ://crypt/…crypt5/` (зашифрованные ссылки), заголовков
  провайдера (`providerid`, `routing`, `change-user-agent`, `manual-block-user-agent`, …).

Под Happ Remnawave отдаёт `XRAY_BASE64` или `XRAY_JSON` (если включено
`serveJsonAtBaseSubscription`; Happ в списке `JSON_SUBSCRIPTION_FALLBACK_CLIENTS`).
Remnawave XRAY_JSON — это **массив** полных Xray-конфигов, по одному на хост, с `remarks`
и outbound'ом `tag: "proxy"` (`xray-json.generator.service.ts`).

Что mihomyak делает так же, как оригинал, но не проверено захватом (помечено в коде):
`Accept-Language` для локалей, кроме `C`/`en` и `ru_RU`; `X-Ver-Os` для дистрибутивов без
`ID`/`VERSION_ID` (Qt вернёт `unknown`).

Android-версия Happ (по сторонним данным, **не проверено**): `User-Agent: Happ/<ver>`,
`X-Device-Os: Android`, `X-Ver-Os: <android>`, `X-Device-Model: <model>`,
`X-Hwid: <16 hex>`, `X-Device-Locale`, `X-Real-Ip`, `X-Forwarded-For`.

---

### 7.4 Один machine-id — разные HWID

FlClashX шлёт `sha256(machine-id)[:16]` в **верхнем** регистре, Koala — в **нижнем**,
Happ — сырой machine-id. Remnawave сравнивает HWID как строку, поэтому **смена
эмулируемого клиента на той же машине занимает новый слот устройства** (проверено
на мок-панели с лимитом 1). Выберите клиента один раз.

## 8. Что осталось за рамками точной имитации

- **TLS-отпечаток (JA3/JA4).** Dart (BoringSSL) и Node (OpenSSL) дают разные ClientHello;
  mihomyak использует rustls. Панели этого не проверяют, но антибот-прокси перед
  панелью (Cloudflare, DDoS-Guard) теоретически могут.
- **ALPN.** Ни Dart HttpClient, ни Node https (axios) по умолчанию не шлют ALPN,
  mihomyak тоже. HTTP/2 не используется никем из трёх.
- **Сжатие.** mihomyak заявляет ровно те кодировки, что и оригинал, и умеет их
  распаковывать (`gzip`, `deflate`, `br`, `zstd`; `compress` (LZW) у Koala объявлен,
  но серверы его не используют).
- **Зашифрованные ссылки `happ://crypt…/`** не расшифровываются: нужна обычная https-ссылка.
- **Xray JSON → mihomo** теряет то, чего нет в mihomo: `sockopt.dialerProxy` (цепочки,
  fragment), Xray mux, транспорт kcp, finalmask hysteria. Такие хосты пропускаются или
  конвертируются без опции, с предупреждением в логе.
- **IDN-домены** (`.рф`) не поддерживаются: нужен punycode.
- **Управляющие символы** (CR, LF, TAB) в значениях заголовков не отправляются:
  настоящий клиент на них упал бы, а mihomyak их вырезает (например, os-release с
  окончаниями строк CRLF).

## 9. Практические выводы (что реализовано в mihomyak)

1. UA и HWID-заголовки строго по формулам клиента, с тем же регистром и порядком.
2. HWID выводится из «machine-id». В Docker его нет, поэтому он генерируется один раз
   и хранится в `/data/machine-id`. Можно задать явно, чтобы перенести устройство.
3. HWID проверяется регуляркой Remnawave `^[a-zA-Z0-9=-]{10,64}$` до отправки.
4. Заглушка определяется по заголовкам `x-hwid-*` и по серверам `0.0.0.0` / `127.0.0.1`
   / порт ≤ 1. Заглушка не заменяет последний рабочий конфиг.
5. Интервал обновления берётся из `profile-update-interval` (как у всех трёх клиентов),
   по умолчанию 24 ч (как у FlClashX).
6. `flclashx-newdomain` обрабатывается как у FlClashX: новый хост сохраняется.
7. Режим маршрутизации принадлежит клиенту (как у FlClashX): `mode: global` из шаблона
   Remnawave игнорируется, иначе трафик ушёл бы через `GLOBAL → DIRECT`.
8. Xray JSON конвертируется по той же таблице полей, что использует генератор mihomo
   в самом Remnawave (`mihomo.generator.service.ts`), и проверяется `mihomo -t` в тестах.
   Привязка сертификата (`pinnedPeerCertSha256`) переносится в `fingerprint` mihomo,
   reality без открытого ключа пропускается.
9. Подписка — недоверенный вход. Из неё берётся только белый список ключей (прокси,
   группы, провайдеры, правила, политика DNS), небезопасные типы прокси
   (tailscale, zerotier, …) выбрасываются, провайдеры не могут выбрать путь файла.
   Каждый новый конфиг проверяется `mihomo -t` до применения, а если работающий
   mihomo всё же отверг его при перезагрузке, возвращаются файлы `*.prev`.
10. Редирект с https на http не выполняется (токен и HWID ушли бы открытым текстом);
    `flclashx-newdomain` принимается только из ответа по https и только как голое
    имя хоста.
