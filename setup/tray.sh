#!/usr/bin/env bash
# Трей hp-tray и hpctl на машине клиента (Linux x86_64 с рабочим столом), чтобы смотреть статус
# её vps-client прямо на ней. Собирает hp-tray и hpctl здесь, копирует по ssh в ~/.local/bin пользователя
# ssh, ставит user-службу hp-tray.service (+ ярлык автозапуска для сессий без systemd), включает
# управление у vps-client (CONTROL_ADDR=127.0.0.1:47001 в /etc/vps-client/vps-client.conf, если не
# задан) и кладёт строку подключения в ~/.config/home-proxy/tray.conf. Туннель не нужен: трей и служба
# на одной машине. Клиент уже должен быть поставлен: setup/vps-client.sh <клиент> [сервер].
#
#   setup/tray.sh <ssh-алиас клиента>
#
# Недостающую libxkbcommon-x11 (dlopen) ставит через apt-get.
# Пользователь ssh — тот, кто входит в рабочий стол клиента (трей — его user-служба, не root).
# Переменные (необязательно): CONTROL_ADDR (127.0.0.1:47001), DRY_RUN=1 — только проверки и сборка.
set -euo pipefail
. "$(dirname "$0")/common.sh"

CLI="${1:?использование: $0 <ssh-алиас клиента>}"
CONTROL_ADDR="${CONTROL_ADDR:-127.0.0.1:47001}"
CONF=/etc/vps-client/vps-client.conf

step "Клиент $CLI: проверка"
arch="$(ssh_to "$CLI" 'uname -m')" || die "нет доступа по ssh к $CLI"
[[ "$arch" == "x86_64" ]] || die "архитектура $arch: трей ставится только на Linux x86_64 с рабочим столом (на роутере — hpctl/LuCI)"
[[ "$(ssh_to "$CLI" 'id -u')" != "0" ]] || die "ssh на $CLI идёт под root: трей — user-служба пользователя рабочего стола, зайдите под ним"
sudo_cmd="sudo -n"
ssh_to "$CLI" 'sudo -n true' || die "на $CLI нет sudo без пароля (sudo -n): нужен для чтения $CONF и control.key"
ssh_to "$CLI" "sudo -n test -f $CONF" || die "на $CLI нет $CONF: сначала setup/vps-client.sh $CLI"
ssh_to "$CLI" 'command -v systemctl >/dev/null' || die "на $CLI нет systemd"

require_build_tools_light() {
  command -v cargo >/dev/null || die "нет cargo: установите rustup (https://rustup.rs)"
  command -v protoc >/dev/null || die "нет protoc: пакет protobuf-compiler"
  command -v objdump >/dev/null || die "нет objdump: пакет binutils"
}
require_build_tools_light

step "Сборка hp-tray и hpctl (x86_64)"
(cd "$ROOT" && cargo build --release -p hp-tray -p hp-control)
tray_bin="$ROOT/target/release/hp-tray"
hpctl_bin="$ROOT/target/release/hpctl"
[[ -x "$tray_bin" && -x "$hpctl_bin" ]] || die "не нашёл собранные hp-tray/hpctl в $ROOT/target/release"
cli_glibc="$(ssh_to "$CLI" 'ldd --version | head -1 | grep -oE "[0-9]+\.[0-9]+$"')"
check_glibc "$tray_bin" "$cli_glibc"
check_glibc "$hpctl_bin" "$cli_glibc"
[[ "${DRY_RUN:-}" != "1" ]] || { echo "DRY_RUN=1: сборка и проверки пройдены, на $CLI ничего не менялось"; exit 0; }

step "Клиент $CLI: управление у vps-client (CONTROL_ADDR=$CONTROL_ADDR)"
if ssh_to "$CLI" "$sudo_cmd grep -q '^CONTROL_ADDR=' $CONF"; then
  echo "CONTROL_ADDR уже задан: $(ssh_to "$CLI" "$sudo_cmd sed -n 's/^CONTROL_ADDR=//p' $CONF")"
else
  ssh_to "$CLI" "$sudo_cmd sh -c 'echo CONTROL_ADDR=$CONTROL_ADDR >> $CONF'"
  ssh_to "$CLI" "$sudo_cmd systemctl restart hp-vps-client"
  echo "CONTROL_ADDR добавлен в $CONF, hp-vps-client перезапущен (дыры поднимутся заново за несколько секунд)"
fi

step "Клиент $CLI: установка hp-tray и hpctl"
ssh_to "$CLI" 'mkdir -p ~/.local/bin ~/.config/systemd/user ~/.config/autostart ~/.config/home-proxy'
for pair in "hp-tray:$tray_bin" "hpctl:$hpctl_bin"; do
  name="${pair%%:*}"; src="${pair#*:}"
  ssh_to "$CLI" "cat > ~/.local/bin/$name.new && chmod 755 ~/.local/bin/$name.new && mv -f ~/.local/bin/$name.new ~/.local/bin/$name" < "$src"
done
ssh_to "$CLI" 'cat > ~/.config/systemd/user/hp-tray.service' < "$ROOT/control/tray/hp-tray.service"
ssh_to "$CLI" 'cat > ~/.config/autostart/hp-tray.desktop' < "$ROOT/control/tray/hp-tray-autostart.desktop"
# ярлык с окном: в меню приложений и на рабочем столе (значок в трее есть не в каждой среде)
ssh_to "$CLI" 'mkdir -p ~/.local/share/applications; cat > ~/.local/share/applications/hp-tray.desktop' < "$ROOT/control/tray/hp-tray.desktop"
ssh_to "$CLI" 'sed -i "s#^Exec=.*#Exec=$HOME/.local/bin/hp-tray#" ~/.local/share/applications/hp-tray.desktop
  d="$(xdg-user-dir DESKTOP 2>/dev/null || true)"; [ -n "$d" ] || d=~/Desktop
  if [ -d "$d" ]; then cp ~/.local/share/applications/hp-tray.desktop "$d/hp-tray.desktop" && chmod 755 "$d/hp-tray.desktop" && gio set "$d/hp-tray.desktop" metadata::trusted true 2>/dev/null || true; echo "ярлык: $d/hp-tray.desktop"; fi'
missing="$(ssh_to "$CLI" 'ldd ~/.local/bin/hp-tray | grep "not found" || true')"
# hp-tray подгружает часть библиотек через dlopen (ldd их не видит): без них падает при старте
if ! ssh_to "$CLI" '/sbin/ldconfig -p | grep -q "libxkbcommon-x11\.so"'; then
  if ssh_to "$CLI" 'command -v apt-get >/dev/null'; then
    echo "на $CLI нет libxkbcommon-x11: ставлю libxkbcommon-x11-0"
    ssh_to "$CLI" "$sudo_cmd env DEBIAN_FRONTEND=noninteractive apt-get install -y libxkbcommon-x11-0 >/dev/null" \
      || warn "не удалось поставить libxkbcommon-x11-0 на $CLI: hp-tray упадёт при старте"
  else
    warn "на $CLI нет libxkbcommon-x11 (и нет apt-get): поставьте пакет вручную, иначе hp-tray упадёт при старте"
  fi
fi
[[ -z "$missing" ]] || warn "на $CLI не хватает библиотек для hp-tray (поставьте пакеты): $missing"

step "Клиент $CLI: строка подключения → tray.conf"
conn="$(ssh_to "$CLI" "$sudo_cmd /usr/local/bin/vps-client --config $CONF --connection-string")"
[[ "$conn" == homeproxy-control://* ]] || die "vps-client не напечатал строку подключения: $conn"
# строка — секрет: передаём через stdin, не в аргументах команды
printf '%s\n' "$conn" | ssh_to "$CLI" 'read -r l; f=~/.config/home-proxy/tray.conf; touch "$f"; chmod 600 "$f"; grep -qxF "$l" "$f" || echo "$l" >> "$f"'
echo "строка записана в ~/.config/home-proxy/tray.conf"

step "Клиент $CLI: user-служба hp-tray"
ssh_to "$CLI" 'export XDG_RUNTIME_DIR=/run/user/$(id -u); systemctl --user daemon-reload && systemctl --user enable hp-tray.service 2>&1 | tail -n 1' \
  || warn "не удалось включить hp-tray.service: пользователь $CLI без user-сессии systemd (войдите на рабочем столе клиента один раз)"
if ssh_to "$CLI" 'export XDG_RUNTIME_DIR=/run/user/$(id -u); systemctl --user restart hp-tray.service && sleep 3 && systemctl --user is-active hp-tray.service' >/dev/null 2>&1; then
  echo "hp-tray.service запущена"
else
  warn "hp-tray.service не запустилась по ssh (нет графической сессии: DISPLAY/Wayland). Это нормально: она стартует при входе на рабочем столе клиента, либо вручную там: systemctl --user start hp-tray.service"
fi

step "Готово"
echo "клиент $CLI: трей ~/.local/bin/hp-tray (запуск при входе на рабочем столе)"
echo "статус из консоли на клиенте: hpctl --connect \"\$(head -n 1 ~/.config/home-proxy/tray.conf)\" status"
