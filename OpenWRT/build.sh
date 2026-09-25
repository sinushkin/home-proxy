#!/usr/bin/env bash
# Кросс-сборка peer под OpenWrt (по умолчанию mipsel, Xiaomi 4C / mt76x8).
#
#   TOOLCHAIN_DIR=/путь/к/staging_dir/toolchain-mipsel_24kc_gcc-12.3.0_musl ./OpenWRT/build.sh
#
# Таргеты MIPS в Rust — Tier 3: готового std нет, он собирается из rust-src
# (`-Z build-std`, на stable через RUSTC_BOOTSTRAP=1; нужен компонент rust-src).
# Линкует и собирает C-части зависимостей (ring) gcc из тулчейна OpenWrt.
#
# Другой роутер: TARGET и GCC_PREFIX, например ath79 (mips, big-endian):
#   TARGET=mips-unknown-linux-musl GCC_PREFIX=mips-openwrt-linux-musl \
#   TOOLCHAIN_DIR=.../toolchain-mips_24kc_gcc-11.2.0_musl ./OpenWRT/build.sh
set -euo pipefail

: "${TOOLCHAIN_DIR:?задайте TOOLCHAIN_DIR — каталог toolchain-* из staging_dir OpenWrt}"
TARGET="${TARGET:-mipsel-unknown-linux-musl}"
GCC_PREFIX="${GCC_PREFIX:-mipsel-openwrt-linux-musl}"

# Собираем из workspace hp-backend/ (рядом с этой папкой).
cd "$(dirname "${BASH_SOURCE[0]}")/../hp-backend"
export STAGING_DIR="${STAGING_DIR:-$(dirname "$TOOLCHAIN_DIR")}"
export PATH="$TOOLCHAIN_DIR/bin:$PATH"

upper="$(echo "$TARGET" | tr 'a-z-' 'A-Z_')"
under="${TARGET//-/_}"
export "CARGO_TARGET_${upper}_LINKER=${GCC_PREFIX}-gcc"
export "CARGO_TARGET_${upper}_RUSTFLAGS=-C link-self-contained=no"
export "CC_${under}=${GCC_PREFIX}-gcc" "AR_${under}=${GCC_PREFIX}-ar"

# Размер важен: на роутере мало флеша.
export CARGO_PROFILE_RELEASE_OPT_LEVEL=z CARGO_PROFILE_RELEASE_LTO=fat
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 CARGO_PROFILE_RELEASE_PANIC=abort
export CARGO_PROFILE_RELEASE_STRIP=true

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target/openwrt}"
export RUSTC_BOOTSTRAP=1
# Что собираем: peer (клиент) и router (релей телефон <-> сервер). Один пакет:
# PACKAGES=router ./OpenWRT/build.sh
PACKAGES="${PACKAGES:-peer router}"
args=()
for package in $PACKAGES; do args+=(-p "$package"); done
cargo build --release "${args[@]}" --target "$TARGET" -Zbuild-std=std,panic_abort

for package in $PACKAGES; do
  bin="$CARGO_TARGET_DIR/$TARGET/release/$package"
  ls -la "$bin"
  file "$bin" | cut -c1-160
done
