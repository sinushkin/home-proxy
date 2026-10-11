# Сборка для OpenWrt (роутеры MIPS): с нуля

Короткий путь для новичка: как собрать наши программы (`vps-client`, `peer`, `hp-router`) под
роутер на OpenWrt, что для этого нужно и почему это вообще работает.

## Главное, что нужно понять

Rust умеет собирать под MIPS, просто это не «нажал кнопку». Тут три вещи:

1. **Rust не поставляет готовую стандартную библиотеку (`std`) для MIPS.** Для `mipsel` это
   «Tier 3»: компилятор знает такой таргет, но готовых пакетов нет. Поэтому `std` компилируется
   из исходников (компонент `rust-src`). Это делает сам `cargo`, отдельного шага нет. Первая
   сборка — несколько минут, потом кэш.
2. **Нужен компилятор и библиотека C от OpenWrt**, а не от твоего ПК. Бинарник должен
   линковаться с той же `libc` (musl), что стоит на роутере. Этот набор называется **тулчейн**
   (toolchain) и лежит в **SDK** OpenWrt.
3. **MIPS32 не умеет 64-битные атомарные операции** и не имеет SIMD. В коде мы этого не используем
   (`AtomicU32`, не `AtomicU64`), проверка — ниже.

## Что понадобится

| Что | Зачем | Как получить |
|---|---|---|
| Rust (`rustup`) с `rust-src` | компилятор и исходники `std` | `rustup component add rust-src` |
| Тулчейн OpenWrt (`staging_dir/toolchain-mipsel_…`) | компилятор C и линкер для MIPS | SDK для нужной версии OpenWrt и твоего роутера |
| `protoc` | генерация кода из `.proto` | пакет `protobuf-compiler` (Debian/Ubuntu) |

**Тулчейн.** Скачай SDK с <https://downloads.openwrt.org> в папке для своей версии OpenWrt и
архитектуры (`releases/<версия>/targets/ramips/mt76x8/` для Xiaomi 4C). Внутри SDK каталог
`staging_dir/toolchain-mipsel_24kc_…`. Тулчейн должен быть от той же версии OpenWrt, что стоит
на роутере: тогда версия `libc` совпадает. Для Xiaomi 4C мы использовали
`toolchain-mipsel_24kc_gcc-12.3.0_musl` (OpenWrt 23.05), и на 25.12 бинарник работал без
пересборки.

## Шаг 1. Rust и исходники std

```bash
# Установить rustup (если нет): https://rustup.rs
rustup component add rust-src
```

Проверка: `rustc --version` должен работать из обычного терминала.

## Шаг 2. Тулчейн OpenWrt

Пусть SDK лежит в `~/owrt`. Тогда:

```bash
export TOOLCHAIN_DIR=~/owrt/staging_dir/toolchain-mipsel_24kc_gcc-12.3.0_musl
$TOOLCHAIN_DIR/bin/mipsel-openwrt-linux-musl-gcc --version   # должна быть версия gcc
```

## Шаг 3. Сборка наших программ

Из корня репозитория:

```bash
TOOLCHAIN_DIR=~/owrt/staging_dir/toolchain-mipsel_24kc_gcc-12.3.0_musl \
PACKAGES=vps-client ./OpenWRT/build.sh
```

Скрипт `OpenWRT/build.sh` сам выставляет все переменные окружения (линкер, размер бинарника,
таргет). Первый запуск долгий, потому что собирается `std`.

Что собирать, переменная `PACKAGES`:
- `vps-client` — клиент VPS-сервера (обычный роутер или любой Linux на MIPS);
- `peer` — P2P-клиент (два пира, MQTT, STUN);
- `hp-router` — роутер «дом ↔ VPS + телефоны» (две роли в одном процессе).

Без `PACKAGES` собираются все три.

**Где бинарник:** `target/openwrt/mipsel-unknown-linux-musl/release/<имя>`. Примерно 2 МБ.

## Шаг 4. Проверки перед установкой

```bash
B=target/openwrt/mipsel-unknown-linux-musl/release/vps-client
file $B                              # ELF 32-bit LSB ... MIPS, MIPS32 ...
objdump -T $B | grep -c "__atomic_.*_8"   # должно быть 0
objdump -p $B | grep NEEDED          # libc.so, libgcc_s.so.1
```

- `__atomic_*_8` больше нуля — в бинарнике 64-битные атомики, на MIPS32 он не заработает.
- В `NEEDED` должны быть только `libc.so` (musl) и `libgcc_s.so.1`: они есть на OpenWrt.

## Шаг 5. Установка на роутер

Ниже пример, как мы ставили на Xiaomi 4C (`jump1`). Адрес роутера подставь свой.

```bash
# Копия через ssh (scp на dropbear не всегда работает, поэтому через cat)
ssh root@<роутер> 'cat > /usr/bin/vps-client && chmod 755 /usr/bin/vps-client' < $B
```

Конфиг и автозапуск — отдельно, см. `templates/vps-client-server/README.md` и
`OpenWRT/luci-app-homeproxy/root/etc/init.d/hp-router` (пример procd-скрипта).

## Другой роутер

Если роутер с другим процессором (например, ath79 — big-endian MIPS), меняются две переменные:

```bash
TARGET=mips-unknown-linux-musl GCC_PREFIX=mips-openwrt-linux-musl \
TOOLCHAIN_DIR=~/owrt/staging_dir/toolchain-mips_24kc_gcc-11.2.0_musl \
PACKAGES=vps-client ./OpenWRT/build.sh
```

Как узнать, что у роутера: `cat /proc/cpuinfo` и `ls /lib/ld-musl-*` (имя загрузчика покажет
порядок байтов и soft/hard-float). Для `ld-musl-mipsel-sf` нужен `mipsel-…`, для `ld-musl-mips-sf`
— `mips-…`. Тулчейн должен совпадать с ними.

## Частые проблемы

- **`can't find crate for core` или `the option -Z build-std …`.** Не стоит `rust-src` или
  `RUSTC_BOOTSTRAP`. Скрипт выставляет `RUSTC_BOOTSTRAP=1` сам, `rust-src` ставится шагом 1.
- **`undefined reference to __atomic_*_8`.** В коде или зависимости появился 64-битный атомик.
  Найти место: `grep -rn "AtomicU64\|AtomicI64" --include=*.rs`.
- **Бинарник не запускается на роутере (`not found`).** Загрузчик или `libc` не совпадают с
  роутером. Проверь `ls /lib/ld-musl-*` и версию SDK.
- **Долго собирается.** Так и должно быть при первой сборке (`std` с нуля). Последующие быстрее.

## Почему это не «магия»

Всё, что делает `build.sh`, можно сделать руками: это четыре переменные окружения и одна команда
`cargo build --target mipsel-unknown-linux-musl -Zbuild-std=std,panic_abort`. Подробный разбор
со всеми переменными — в `OpenWRT/README.md`.
