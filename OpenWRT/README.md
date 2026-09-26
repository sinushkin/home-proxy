# Сборка `peer` под OpenWrt

Как собрать `peer` для роутера на OpenWrt (проверено на `mipsel_24kc`, это
Xiaomi 4C / mt76x8), какие у этого особенности и как повторить сборку вручную.

Быстро (скрипт делает всё описанное ниже):

```bash
TOOLCHAIN_DIR=~/owrt/staging_dir/toolchain-mipsel_24kc_gcc-12.3.0_musl ./OpenWRT/build.sh
# результат: target/openwrt/mipsel-unknown-linux-musl/release/{hp-router,peer,vps-client} (~2 МБ каждый);
# серверы (`vps-server`, `hp-server`) — обычные x86_64-бинарники, под роутер их не собирают
```

## Что нужно

| Что | Зачем | Где взять |
|---|---|---|
| Rust из **rustup** (`rustc`, `cargo`) с компонентом `rust-src` | собрать `std` под целевой таргет | `rustup component add rust-src`; `~/.cargo/bin` должен стоять в `PATH` раньше `/usr/bin` (системный `cargo` из Debian без `rust-src` не подходит) |
| Тулчейн OpenWrt (`staging_dir/toolchain-*`) | линкер и C-компилятор (`ring`, часть TLS, написана на C) | ниже |
| `protoc` в `PATH` | `prost-build` (как и в обычной сборке) | пакет `protobuf-compiler` |
| Сеть при первой сборке | `cargo` скачивает зависимости `std` (`compiler_builtins` и др.) | |

Проверено на: `rustc 1.96.0` (stable), OpenWrt GCC `12.3.0 r24012-d8dd03c46f`.

**Тулчейн.** Он уже лежит на виртуальной машине `ot`: в `~/openwrt/staging_dir/`
(mipsel, gcc 12.3.0, ~255 МБ — этот использован), а также
`~/openwrt-mt7621/staging_dir/` и `~/openwrt-ath79/staging_dir/` (gcc 11.2.0).
Компилятор — обычные x86_64-бинарники, они запускаются и на другой Linux-машине,
поэтому Rust на `ot` ставить не нужно: тулчейн копируется туда, где стоит Rust
(каталог `toolchain-*` целиком, вместе со `staging_dir` наверху — его путь нужен
как `STAGING_DIR`):

```bash
mkdir -p ~/owrt/staging_dir
rsync -a ot:~/openwrt/staging_dir/toolchain-mipsel_24kc_gcc-12.3.0_musl ~/owrt/staging_dir/
~/owrt/staging_dir/toolchain-mipsel_24kc_gcc-12.3.0_musl/bin/mipsel-openwrt-linux-musl-gcc --version
```

Проверка тулчейна (получить MIPS-исполняемый файл):

```bash
export STAGING_DIR=~/owrt/staging_dir
echo 'int main(){return 0;}' > /tmp/t.c
$STAGING_DIR/toolchain-mipsel_24kc_gcc-12.3.0_musl/bin/mipsel-openwrt-linux-musl-gcc /tmp/t.c -o /tmp/t.out
file /tmp/t.out     # ELF 32-bit LSB executable, MIPS, MIPS32 rel2 ...
```

## Особенности (почему нельзя просто `--target`)

1. **Готового `std` для MIPS нет.** `mips*-unknown-linux-musl` в Rust — Tier 3:
   компилятор знает таргет, но бинарных пакетов `rust-std` не публикуют ни
   в stable, ни в nightly (проверено по манифестам дистрибутива), и
   `rustup target add` не работает. `std` собирается из исходников
   (`rust-src`) опцией `-Z build-std`.
2. **`-Z build-std` нестабилен**, поэтому на stable нужен
   `RUSTC_BOOTSTRAP=1` (разрешает нестабильные опции без nightly). Можно и
   nightly, тогда переменная не нужна.
3. **Нет 64-битных атомиков.** У 32-битных MIPS `max-atomic-width = 32`, тип
   `AtomicU64` недоступен (ошибка компиляции). В нашем коде счётчики в
   `punch.rs` — `AtomicU32`; 64-битных атомиков не должно быть нигде. tokio
   на таких таргетах эмулирует 64-битные счётчики сам.
4. **Нет FPU и нет SIMD.** Таргет `mipsel-unknown-linux-musl`:
   `mips32r2 + soft-float`, как и OpenWrt `mipsel_24kc`. Векторные расширения
   MIPS (MSA) есть только в архитектуре R5, ядра 24Kc/24KEc/1004Kc их не имеют.
   XOR (`connection/src/xor.rs`) на MIPS идёт базовой веткой (обычные 32-битные
   операции), AVX2/AVX-512 включаются только на x86_64.
5. **Линкуем gcc из тулчейна OpenWrt**, а не самодостаточным musl из Rust:
   для Tier 3 своих `crt`/`libc.a` в поставке Rust нет (`-C link-self-contained=no`).
   Бинарник получается динамическим (`/lib/ld-musl-mipsel-sf.so.1`, `libgcc_s.so.1`);
   на OpenWrt оба есть по умолчанию.
6. **`ring` (TLS) собирается на MIPS** без правок: его C-части компилирует тот же
   gcc (`CC_<таргет>`, `AR_<таргет>`). Запасной вариант с OpenSSL не понадобился.
7. **Логи — в syslog.** `LOG_TARGET=syslog` пишет в `syslog(3)`; на OpenWrt его
   принимает `logd`, читать `logread -e peer`. Так же пишет `ulog` из libubox.
8. **Без `regex`.** `env_logger` в `Cargo.toml` подключён с
   `default-features = false, features = ["auto-color", "humantime"]`: фильтр по
   модулям (`RUST_LOG=peer=debug,connection=info`) остаётся, а regex-фильтр по
   тексту убран вместе с крейтами `regex`, `regex-automata`, `regex-syntax`,
   `aho-corasick` (около четверти кода). Это для всех таргетов, не только MIPS.

## Как собирается `std` и как собрать проект вручную

`-Z build-std=std,panic_abort` заставляет `cargo` собрать `core`, `alloc`, `std`,
`panic_abort` и их зависимости из `$(rustc --print sysroot)/lib/rustlib/src/rust/library`
под нужный таргет, с теми же настройками профиля (`opt-level=z`, LTO и т.д. —
поэтому `std` тоже получается компактным). Готовый `std` при этом лежит не в
sysroot, а в каталоге сборки (`CARGO_TARGET_DIR/<таргет>/release/deps`) и
пересобирается для каждого нового каталога (несколько десятков секунд).
Отдельного шага «собрать `std`» нет: он часть команды `cargo build`.
`panic_abort` нужен, потому что профиль `panic = "abort"`.

Пошагово, без скрипта (из корня репозитория):

```bash
# 1. Rust: исходники std
rustup component add rust-src

# 2. Тулчейн OpenWrt (см. выше) и переменные окружения
export STAGING_DIR=~/owrt/staging_dir
export TOOLCHAIN_DIR=$STAGING_DIR/toolchain-mipsel_24kc_gcc-12.3.0_musl
export PATH=$TOOLCHAIN_DIR/bin:$PATH

# 3. Кто линкует и компилирует C (имена переменных — по таргету, дефисы -> '_')
export CARGO_TARGET_MIPSEL_UNKNOWN_LINUX_MUSL_LINKER=mipsel-openwrt-linux-musl-gcc
export CARGO_TARGET_MIPSEL_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C link-self-contained=no"
export CC_mipsel_unknown_linux_musl=mipsel-openwrt-linux-musl-gcc
export AR_mipsel_unknown_linux_musl=mipsel-openwrt-linux-musl-ar

# 4. Профиль «минимальный размер» (без правки Cargo.toml, только для этой сборки)
export CARGO_PROFILE_RELEASE_OPT_LEVEL=z
export CARGO_PROFILE_RELEASE_LTO=fat
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1
export CARGO_PROFILE_RELEASE_PANIC=abort
export CARGO_PROFILE_RELEASE_STRIP=true

# 5. Сборка (RUSTC_BOOTSTRAP — чтобы -Z работал на stable)
cd <корень репозитория>
RUSTC_BOOTSTRAP=1 cargo build --release -p peer \
  --target mipsel-unknown-linux-musl \
  -Zbuild-std=std,panic_abort

# 6. Результат
ls -la target/mipsel-unknown-linux-musl/release/peer
file    target/mipsel-unknown-linux-musl/release/peer
$TOOLCHAIN_DIR/bin/mipsel-openwrt-linux-musl-readelf -d target/mipsel-unknown-linux-musl/release/peer | grep NEEDED
```

Ожидаемо: `ELF 32-bit LSB pie executable, MIPS, MIPS32 rel2`, интерпретатор
`/lib/ld-musl-mipsel-sf.so.1`, зависимости `libgcc_s.so.1` и `libc.so`.
`OpenWRT/build.sh` делает то же самое и кладёт результат в
`target/openwrt/...` (отдельный каталог сборки, чтобы не мешать
обычным сборкам).

Что означают переменные:

| Переменная | Смысл |
|---|---|
| `STAGING_DIR` | каталог, где лежит `toolchain-*`; gcc OpenWrt его ожидает |
| `CARGO_TARGET_<ТАРГЕТ>_LINKER` | чем линковать (gcc из тулчейна) |
| `CARGO_TARGET_<ТАРГЕТ>_RUSTFLAGS` | `link-self-contained=no`: не искать `crt` в поставке Rust |
| `CC_<таргет>`, `AR_<таргет>` | чем собирать C-код зависимостей (`ring`) |
| `CARGO_PROFILE_RELEASE_*` | профиль релиза под размер, без правки `Cargo.toml` |
| `RUSTC_BOOTSTRAP=1` | разрешить `-Z build-std` на stable |

## Размер

| Сборка | Размер (strip) |
|---|---|
| обычный `--release` для mipsel | 5.07 МБ |
| + size-профиль (`opt-level=z`, LTO, `panic=abort`) | 2.43 МБ |
| + без `regex` в `env_logger` | **1.71 МБ** |

Что ещё можно урезать (не сделано): фичи `tokio` (вместо `full` только
`rt`, `net`, `time`, `sync`, `io-util`, `io-std`, `macros`; для одного ядра
без `rt-multi-thread`).

## Запуск на роутере

Нужны бинарник `peer` и `ca.crt` брокера (публичный сертификат). Файлы на
роутер копируются `scp -O` (у dropbear нет sftp, как и в `client-c/deploy.sh`):

```bash
scp -O target/mipsel-unknown-linux-musl/release/peer root@<роутер>:/tmp/peer
scp -O cert/out/ca.crt root@<роутер>:/tmp/ca.crt
```

Запуск (логи в syslog, читать `logread -e peer`):

```sh
LOG_TARGET=syslog RUST_LOG=peer=info,connection=info \
  /tmp/peer <stun_ip:3499> <mqtt_ip:8883> /tmp/ca.crt <мой-guid> <guid-пира> </dev/null &
logread -f -e peer
```

Ввод с клавиатуры на роутере обычно не нужен (`</dev/null`: `peer` при закрытом
stdin просто отключает ввод и работает). Автозапуск через procd — обычный
init-скрипт (`/etc/init.d/peer`, `procd_set_param command`, `env LOG_TARGET=syslog`).
Этот скрипт в репозитории не проверялся.

## Другие роутеры

| Роутер | Таргет Rust | Префикс gcc (`GCC_PREFIX`) | Тулчейн |
|---|---|---|---|
| Xiaomi 4C (mt76x8), проверено | `mipsel-unknown-linux-musl` | `mipsel-openwrt-linux-musl` | `toolchain-mipsel_24kc_gcc-12.3.0_musl` |
| mt7621 (Redmi 4A) | `mipsel-unknown-linux-musl` | `mipsel-openwrt-linux-musl` | `toolchain-mipsel_24kc_gcc-11.2.0_musl` |
| ath79 (mips, big-endian) | `mips-unknown-linux-musl` | `mips-openwrt-linux-musl` | `toolchain-mips_24kc_gcc-11.2.0_musl` |

Для второй и третьей строки `build.sh` принимает `TARGET` и `GCC_PREFIX`:

```bash
TARGET=mips-unknown-linux-musl GCC_PREFIX=mips-openwrt-linux-musl \
TOOLCHAIN_DIR=~/owrt/staging_dir/toolchain-mips_24kc_gcc-11.2.0_musl ./OpenWRT/build.sh
```

Эти две строки **не проверялись** (собирался только mipsel gcc 12.3.0). У
big-endian ath79 интерпретатор будет `ld-musl-mips-sf.so.1`; код от порядка байт
не зависит.

## Проверено на реальном роутере

Роутер за `ssh jump1`: OpenWrt 23.05.4 `r24012-d8dd03c46f` (та же ревизия, что и
у тулчейна gcc 12.3.0), `ramips/mt76x8`, `mipsel_24kc`, MediaTek MT7628AN (MIPS
24KEc), около 57 МБ ОЗУ (свободно порядка 20 МБ), `/tmp` 28 МБ, `/overlay` 9 МБ.
Бинарники залиты в `/tmp/hp/` (оперативная память, во флеш ничего не пишется):

```bash
ssh jump1 'mkdir -p /tmp/hp'
scp -O target/openwrt/mipsel-unknown-linux-musl/release/{hp-router,peer} cert/out/ca.crt jump1:/tmp/hp/
```

`jump1` в `~/.ssh/config` идёт через `ProxyJump`; `scp -O` нужен, потому что у
dropbear нет sftp. `ldd` на роутере (это символическая ссылка на `libc.so` musl)
находит все зависимости:

```text
# ldd /tmp/hp/hp-router       (peer — то же самое)
	/lib/ld-musl-mipsel-sf.so.1 (0x77d94000)
	libgcc_s.so.1 => /lib/libgcc_s.so.1 (0x77bb2000)
	libc.so => /lib/ld-musl-mipsel-sf.so.1 (0x77d94000)
```

Оба бинарника запускаются на MT7628 (`peer` печатает usage, `hp-router` — свою
ошибку конфигурации при пустом окружении), контрольные суммы на роутере
совпадают с локальными. Сквозной запуск с телефоном через роутер — `Tun.md`.

## Что проверено, а что нет

Проверено: сборка под `mipsel` без ошибок и предупреждений, тип файла и
зависимости бинарника (`readelf`, `ldd` на роутере), запуск на MT7628, тесты на
хосте, syslog-логгер на хосте (через journald: идентификатор `peer`, PID,
приоритеты), фильтр логов по модулям без `regex`, релей на хосте (два телефона,
роутер, сервер).

Не проверено: работа роутера на самом MT7628 под нагрузкой (связь с телефоном,
память при нескольких телефонах), производительность TLS без аппаратных
ускорителей на 580 МГц, сборки для mt7621 и ath79, init-скрипт procd.

## Если что-то пошло не так

| Симптом | Причина |
|---|---|
| `can't find crate for std` / `core` | забыли `-Zbuild-std=std,panic_abort` или нет `rust-src` |
| `.../library/Cargo.lock does not exist, unable to build with the standard library` | вызван системный `cargo` (Debian), а не rustup: проверьте `which cargo` |
| `the option Z is only accepted on the nightly compiler` | нет `RUSTC_BOOTSTRAP=1` |
| `no method ... AtomicU64` / `cannot find type AtomicU64` | в код попал 64-битный атомик, для MIPS используем `AtomicU32` |
| ошибки `cc`/`ring` про компилятор | не заданы `CC_<таргет>`/`AR_<таргет>` или `PATH` без тулчейна |
| `cannot find crt1.o` при линковке | не заданы `LINKER` и `link-self-contained=no` |
| на роутере `Not found`/`can't execute` | не тот интерпретатор: бинарник под другой ABI (mips vs mipsel, hard/soft float) |

Роутер как шлюз дома к VPS с белым IP и пир для телефонов (`hp-router`, TUN), настройка по шагам
и замеры — [`Tun.md`](Tun.md). Раньше здесь же был WireGuard поверх `vps-client`: на этом роутере
он давал 10–12 Мбит/с, через TUN без него — вдвое больше.
