# Размер бинарников на OpenWrt: что именно «съедает» `hp-router`

Флеш роутера мал (на `o1` MT7621 — 8,3 МБ, после установки свободно ~2,6 МБ; на Xiaomi 4C ещё меньше),
поэтому важно знать, откуда берутся мегабайты. Замер 2026-10-10, `mipsel-unknown-linux-musl`, сборка
`OpenWRT/build.sh` (`opt-level=z`, LTO `fat`, `codegen-units=1`, `panic=abort`, `strip`).

| Бинарник | Файл | Режимы `connection` | Что внутри |
|---|---|---|---|
| `vps-client` | 1 380 584 Б (1,32 МиБ) | только `vps` | дыры к VPS, TUN, управление |
| `hp-router` | 2 363 896 Б (2,25 МиБ) | `vps` + `p2p` | то же + MQTT, TLS, STUN, сопряжение телефонов |
| **разница** | **+983 312 Б (+0,94 МиБ, ×1,7)** | | |

Во флеше (jffs2 сжимает) это дешевле: при добавлении `hp-router` на `o1` занятое место выросло с 4,6
до 5,7 МБ (+1,1 МБ).

## Разбор по крейтам (`cargo bloat --crates`, секция `.text`)

`.text` у `hp-router` 1,8 МиБ против 1,1 МиБ у `vps-client`: разница **717 КиБ** (остальные ~265 КиБ
разницы файла — данные (`.rodata`), таблицы и перемещения тех же крейтов).

| Крейт | `hp-router` | `vps-client` | разница, КиБ |
|---|---|---|---|
| `rustls` (TLS) | 206 | — | **+206** |
| `rumqttc` (MQTT 5) | 127 | — | **+127** |
| `ring` (крипто для TLS) | 105 | — | **+105** |
| `connection` (наш код) | 317 | 238 | +79 |
| `hp_router` (телефоны, пересылка) | 69 | — | +69 |
| `core` (Rust, обобщённый код) | 205 | 140 | +65 |
| `hp_server` (`serve`, настройки, сопряжение) | 52 | — | +52 |
| `[Unknown]` | 38 | — | +38 |
| `webpki` (проверка сертификата) | 32 | — | +32 |
| `hp_control` (управление) | 40 | 28 | +11 |
| `rustls_pemfile`, `rustls_pki_types` | 11 | — | +11 |
| `std`, `tokio`, `prost`, остальное | то же | то же | ~0 |

**Итого:** TLS-стек (`rustls` + `ring` + `webpki` + `pemfile` + `pki_types`) ≈ **355 КиБ**, MQTT-клиент
`rumqttc` ≈ **127 КиБ** — вместе **481 КиБ, две трети всей разницы**. Остальное — наш код P2P и
телефонов (`connection` +79, `hp_router` +69, `hp_server` +52) и обобщённый код `core`.

**STUN почти ничего не весит:** по таблице символов — сотни байт (он тривиален: один запрос и разбор
`XOR-MAPPED-ADDRESS`). Весь наш P2P-код (`connection::p2p`, пробив, обёртка MQTT) — около 50 КиБ.
Виновник — не STUN, а то, что для MQTT по TLS тянется целый TLS-стек и крипто-библиотека.

## Почему `vps-client` маленький

Крейт `connection` делится на режимы по фичам Cargo (`p2p`, `vps`). `vps-client` берёт только `vps`:
без MQTT, TLS и STUN (у сервера белый IP, знакомство идёт на порт сервера). До разделения на режимы он
весил ~2,2 МБ — столько же, сколько сейчас `hp-router`. `hp-router` нужен P2P (телефоны находят роутер
через MQTT по TLS), поэтому TLS из него не убрать, не отказавшись от защищённого рандеву.

⚠ При сборке нескольких пакетов одной командой `cargo` объединяет фичи, и `vps-client` раздуется до
размера `hp-router`. `OpenWRT/build.sh` собирает пакеты по одному — не заменять на общий вызов.

## Как повторить замер

Тот же набор переменных, что в `build.sh`, но **без `strip`** и в отдельном каталоге (чтобы не
затереть боевые бинарники); нужен установленный `cargo-bloat`:

```bash
export TOOLCHAIN_DIR=$HOME/owrt/staging_dir/toolchain-mipsel_24kc_gcc-12.3.0_musl
export STAGING_DIR=$(dirname $TOOLCHAIN_DIR) PATH="$TOOLCHAIN_DIR/bin:$PATH"
export CARGO_TARGET_MIPSEL_UNKNOWN_LINUX_MUSL_LINKER=mipsel-openwrt-linux-musl-gcc
export CARGO_TARGET_MIPSEL_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C link-self-contained=no"
export CC_mipsel_unknown_linux_musl=mipsel-openwrt-linux-musl-gcc AR_mipsel_unknown_linux_musl=mipsel-openwrt-linux-musl-ar
export CARGO_PROFILE_RELEASE_OPT_LEVEL=z CARGO_PROFILE_RELEASE_LTO=fat CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1
export CARGO_PROFILE_RELEASE_PANIC=abort CARGO_PROFILE_RELEASE_STRIP=false
export CARGO_TARGET_DIR=$PWD/target/openwrt-bloat RUSTC_BOOTSTRAP=1
cargo bloat --release --crates -n 45 -p hp-router  --target mipsel-unknown-linux-musl -Zbuild-std=std,panic_abort
cargo bloat --release --crates -n 25 -p vps-client --target mipsel-unknown-linux-musl -Zbuild-std=std,panic_abort
```

`cargo bloat` сам предупреждает, что цифры приблизительные (оценка по символам; inlining и LTO
размывают границы крейтов), так что значения верны с точностью порядка, а не байта.

## Если понадобится уменьшить (идеи, не проверялись)

- Выкинуть из `hp-router` MQTT/TLS нельзя без смены схемы рандеву; MQTT-клиент и TLS можно заменить
  более лёгкими, но это работа над зависимостями, а не настройка.
- `opt-level=z` и `lto=fat` уже включены; дальнейшие флаги (`-Zlocation-detail=none`, `-Zfmt-debug=none`,
  `build-std-features=optimize_for_size`) могут чуть уменьшить файл, выигрыш не измерен.
- Для роутера без телефонов достаточно `vps-client` (1,3 МБ) — шаблон «шлюз без P2P».
