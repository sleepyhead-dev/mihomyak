# Лаборатория: mihomyak как шлюз на живом железе

Стенд для ручной проверки Docker-образа на ARM-машине (проверялось на Raspberry Pi 3B+,
aarch64). Всё создаётся с префиксом `mhk-`, наружу ничего не публикуется, сеть хоста
не меняется: TUN и kill switch работают только в сетевом пространстве контейнера.

```
            mhk-net (10.203.0.0/24, bridge)
 ┌──────────────────────────────────────────────┐
 │ mhk-gw 10.203.0.2  ◀── network_mode ── mhk-app1 (curl)
 │   mihomyak + mihomo, TUN mihomyak0,  ◀──── mhk-app2 (alpine: nslookup, wget)
 │   kill switch                               │
 │ mhk-direct 10.203.0.3  (без шлюза, контроль)│
 │ профиль mock: mhk-panel .10, mhk-node1 .21, mhk-node2 .22 (ss)
 └──────────────────────────────────────────────┘
```

## Образ

Образ собирает CI (артефакт `image-arm64` у каждого PR), компилировать на Pi не нужно:

```sh
gh run download <run-id> -n image-arm64          # на своей машине
scp mihomyak-arm64.tar.gz pi:~/mihomyak-lab/
gunzip -c ~/mihomyak-lab/mihomyak-arm64.tar.gz | docker load   # mihomyak:ci-arm64
```

## Запуск

```sh
cp tests/e2e/lab.env.example ~/mihomyak-lab/lab.env && chmod 600 ~/mihomyak-lab/lab.env
export LAB_ENV=~/mihomyak-lab/lab.env
docker compose -f tests/e2e/lab.compose.yml --profile mock up -d     # мок-панель и узлы
docker compose -f tests/e2e/lab.compose.yml up -d                    # реальная подписка из lab.env
```

С реальной подпиской каждый новый HWID занимает слот устройства у провайдера:
задайте `MIHOMYAK_MACHINE_ID` уже зарегистрированного устройства или смиритесь с новым.
Ссылку держите только в `lab.env` (права `600`), в логи mihomyak она не попадает.

## Что проверять

```sh
docker exec mhk-gw mihomyak status                  # подписка, узлы, ядро
docker exec mhk-app1 curl -s https://ifconfig.me    # IP выхода через шлюз
docker exec mhk-direct curl -s https://ifconfig.me  # IP хоста (должен отличаться)
docker exec mhk-app2 nslookup example.com           # fake-ip 198.18.x.x
docker exec mhk-gw iptables -S MIHOMYAK             # цепочка kill switch
```

Kill switch: `docker exec mhk-gw sh -c 'kill -STOP $(pidof mihomyak); kill -9 $(pidof mihomo)'`
замораживает супервизор и убивает ядро. Запросы `mhk-app1` наружу должны
отклоняться (`Connection refused`), а не уходить напрямую. Разморозка:
`docker exec mhk-gw sh -c 'kill -CONT $(pidof mihomyak)'` (ядро поднимется примерно через секунду).

В профиле mock панелью управляют так:
`docker exec mhk-app2 wget -qO- --post-data= 'http://10.203.0.10:8080/_control?state=http500'`
(`good`, `expired`, `limited`, `http500`, `broken`).

## Уборка

```sh
docker compose -f tests/e2e/lab.compose.yml --profile mock down -v   # контейнеры, сеть, том mhk-gw-data
```
