# Общие функции для setup/*.sh. Подключается так: . "$(dirname "$0")/common.sh"
# Не запускается само по себе. GUID и состояние — в setup/state/ (в .gitignore, права 600).

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
STATE="$ROOT/setup/state"
SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=10 -o LogLevel=ERROR)
TOOLCHAIN_DIR="${TOOLCHAIN_DIR:-$HOME/owrt/staging_dir/toolchain-mipsel_24kc_gcc-12.3.0_musl}"

step() { echo; echo "== $*"; }
warn() { echo "ВНИМАНИЕ: $*" >&2; }
die() { echo "ОШИБКА: $*" >&2; exit 1; }

# Команда на хосте по ssh-алиасу (напрямую, без ProxyJump). Stdin передаётся дальше — можно
# прислать на хост скрипт через heredoc.
ssh_to() {
  local host="$1"; shift
  ssh "${SSH_OPTS[@]}" -o ProxyJump=none "$host" "$@"
}

# Инструменты для сборки на этой машине (без тулчейна OpenWrt).
require_build_tools() {
  command -v cargo >/dev/null || die "нет cargo: установите rustup (https://rustup.rs)"
  rustup component list --installed 2>/dev/null | grep -q '^rust-src' || die "нет компонента rust-src: rustup component add rust-src"
  command -v protoc >/dev/null || die "нет protoc: пакет protobuf-compiler"
  command -v uuidgen >/dev/null || die "нет uuidgen: пакет uuid-runtime"
  command -v objdump >/dev/null || die "нет objdump: пакет binutils"
}

# Тулчейн OpenWrt для mipsel (роутеры). Если его нет — ошибка с подсказкой, а не тихий сбой.
require_openwrt_toolchain() {
  [[ -x "$TOOLCHAIN_DIR/bin/mipsel-openwrt-linux-musl-gcc" ]] || die "тулчейн OpenWrt не найден: $TOOLCHAIN_DIR/bin/mipsel-openwrt-linux-musl-gcc.
  Скачайте SDK для ramips/mt76x8 с https://downloads.openwrt.org (версия не ниже прошивки роутера),
  распакуйте и задайте TOOLCHAIN_DIR=…/staging_dir/toolchain-mipsel_24kc_gcc-…_musl.
  В BUILD_OPENWRT.md — подробности."
}

# Старшая версия GLIBC в бинарнике должна быть не выше glibc машины, где он запустится.
check_glibc() {
  local binary="$1" host_glibc="$2"
  local need newest
  need="$(objdump -T "$binary" | grep -oE 'GLIBC_[0-9.]+' | sed 's/GLIBC_//' | sort -V | tail -1)"
  newest="$(printf '%s\n%s\n' "$need" "$host_glibc" | sort -V | tail -1)"
  [[ "$newest" == "$host_glibc" ]] || die "бинарник требует glibc $need, на машине $host_glibc. Соберите на машине с более старой glibc."
}

# GUID из setup/state/<файл>; создаёт файл при отсутствии. Переменная — имя поля (SERVER_GUID и т.п.).
ensure_guid() {
  local file="$1" field="$2"
  mkdir -p "$STATE"; chmod 700 "$STATE"
  if [[ ! -f "$file" ]]; then
    (umask 077; echo "$field=$(uuidgen)" > "$file")
  fi
  local value
  value="$(sed -n "s/^$field=//p" "$file")"
  [[ -n "$value" ]] || die "битый файл $file (нет $field)"
  echo "$value"
}
