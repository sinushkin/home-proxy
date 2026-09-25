#!/usr/bin/env bash
# Сборка libhomeproxy.so (Rust, hp-backend/android-lib) под Android NDK и
# раскладка в android-vpn/app/src/main/jniLibs/<abi>/.
#
#   ./build-native.sh                 # debug, ABI: armeabi-v7a x86_64
#   ./build-native.sh --release       # release (размер, strip)
#   ABIS="armeabi-v7a arm64-v8a x86_64" ./build-native.sh --release
#
# Тулчейн — как у build-android.sh: NDK 26.1.10909125, API 24,
# clang из NDK и линкер, и C-компилятор (нужен для ring). Другой NDK:
#   ANDROID_NDK=/путь/к/ndk/xx ./build-native.sh
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BACKEND="$HERE/.."
NDK="${ANDROID_NDK:-$HOME/Android/Sdk/ndk/26.1.10909125}"
API="${ANDROID_API:-24}"
ABIS="${ABIS:-armeabi-v7a x86_64}"
BIN="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin"

MODE=debug
CARGO_FLAGS=()
for arg in "$@"; do
  case "$arg" in
    --release) MODE=release; CARGO_FLAGS=(--release) ;;
    *) echo "Usage: $0 [--release]" >&2; exit 1 ;;
  esac
done
[[ -d "$BIN" ]] || { echo "не найден NDK: $NDK (задайте ANDROID_NDK)" >&2; exit 1; }

# Размер: в приложении лишние мегабайты ни к чему. panic оставляем unwind:
# JNI-обёртка ловит паники (abort уронил бы весь процесс приложения).
if [[ "$MODE" == release ]]; then
  export CARGO_PROFILE_RELEASE_OPT_LEVEL=s CARGO_PROFILE_RELEASE_LTO=fat
  export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 CARGO_PROFILE_RELEASE_STRIP=true
fi

for abi in $ABIS; do
  case "$abi" in
    armeabi-v7a) target=armv7-linux-androideabi;  clang="armv7a-linux-androideabi$API-clang" ;;
    arm64-v8a)   target=aarch64-linux-android;    clang="aarch64-linux-android$API-clang" ;;
    x86_64)      target=x86_64-linux-android;     clang="x86_64-linux-android$API-clang" ;;
    x86)         target=i686-linux-android;       clang="i686-linux-android$API-clang" ;;
    *) echo "неизвестный ABI: $abi" >&2; exit 1 ;;
  esac
  if ! rustup target list --installed | grep -qx "$target"; then
    echo "Rust-таргет $target не установлен: rustup target add $target" >&2
    exit 1
  fi
  upper="$(echo "$target" | tr 'a-z-' 'A-Z_')"
  under="${target//-/_}"
  export "CARGO_TARGET_${upper}_LINKER=$BIN/$clang"
  export "CC_${under}=$BIN/$clang" "AR_${under}=$BIN/llvm-ar"

  echo "== $abi ($target, $MODE)"
  (cd "$BACKEND" && cargo build -p homeproxy-android --target "$target" \
      --target-dir target-android "${CARGO_FLAGS[@]}")

  out="$HERE/app/src/main/jniLibs/$abi"
  mkdir -p "$out"
  cp "$BACKEND/target-android/$target/$MODE/libhomeproxy.so" "$out/"
  ls -la "$out/libhomeproxy.so"
done

# Конфиг WireGuard клиента (ключи из wireguard/out) — в assets: приложение берёт
# его по умолчанию. Внутри приватный ключ, поэтому файл в git не попадает.
WG="${WG_CONF:-$HERE/../wireguard/out/client.conf}"
if [[ -f "$WG" ]]; then
  mkdir -p "$HERE/app/src/main/assets"
  cp "$WG" "$HERE/app/src/main/assets/wg.conf"
  echo "client.conf скопирован в assets/wg.conf"
else
  echo "предупреждение: $WG не найден — выполните wireguard/gen.sh (или вставьте конфиг в приложении)" >&2
fi

# CA брокера — публичный сертификат, кладём в assets (в git не попадает).
CA="$HERE/../cert/out/ca.crt"
if [[ -f "$CA" ]]; then
  mkdir -p "$HERE/app/src/main/assets"
  cp "$CA" "$HERE/app/src/main/assets/ca.crt"
  echo "ca.crt скопирован в assets"
else
  echo "предупреждение: $CA не найден — сгенерируйте сертификаты (cert/README.md)" >&2
fi
