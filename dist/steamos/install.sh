#!/usr/bin/env bash
# glidekm client installer for SteamOS (Desktop Mode, Konsole) or any Linux.
# Installs to ~/.local/bin, a systemd *user* service (runs in Game Mode too),
# and, if needed, a udev rule giving the logged-in user access to /dev/uinput.
#
#   ./install.sh              install / update
#   ./install.sh --uninstall  remove everything
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
BIN_DIR="$HOME/.local/bin"
CONF_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/glidekm"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
UDEV_RULE=/etc/udev/rules.d/60-glidekm-uinput.rules

if [[ "${1:-}" == "--uninstall" ]]; then
  systemctl --user disable --now glidekm-client.service 2>/dev/null || true
  rm -f "$UNIT_DIR/glidekm-client.service" "$BIN_DIR/glidekm-client"
  systemctl --user daemon-reload || true
  echo "Removed binary and service. Config kept in $CONF_DIR."
  [[ -f $UDEV_RULE ]] && echo "To remove the udev rule: sudo rm $UDEV_RULE"
  exit 0
fi

mkdir -p "$BIN_DIR" "$CONF_DIR" "$UNIT_DIR"
install -m 755 "$HERE/glidekm-client" "$BIN_DIR/glidekm-client"
echo "Installed $BIN_DIR/glidekm-client"

if [[ ! -f "$CONF_DIR/client.conf" ]]; then
  read -r -s -p "Passphrase (same as on the PC; empty = generate one): " PASS; echo
  if [[ -z "$PASS" ]]; then
    PASS="$(head -c 18 /dev/urandom | base64 | tr -d '/+=')"
    echo "Generated passphrase: $PASS"
    echo "Put it in glidekm-server.conf on the PC."
  fi
  umask 077
  cat > "$CONF_DIR/client.conf" <<EOF
# glidekm client configuration
passphrase = $PASS
port = 4242
# Uncomment to override the detected screen size (logical, landscape):
# width = 1280
# height = 800
EOF
  echo "Wrote $CONF_DIR/client.conf"
fi

# /dev/uinput access. Steam's own udev rules usually grant it to the active
# user already; otherwise add a tiny rule (needs sudo; on a Deck, set a
# password first with `passwd`).
if [[ ! -w /dev/uinput ]]; then
  echo "No write access to /dev/uinput - installing udev rule (sudo)."
  echo 'KERNEL=="uinput", SUBSYSTEM=="misc", TAG+="uaccess", OPTIONS+="static_node=uinput"' |
    sudo tee "$UDEV_RULE" >/dev/null
  sudo modprobe uinput || true
  sudo udevadm control --reload-rules
  sudo udevadm trigger --name-match=uinput || true
  sleep 1
  if [[ ! -w /dev/uinput ]]; then
    echo "WARNING: /dev/uinput still not writable. Log out and back in (or reboot) and re-run."
  fi
fi

cat > "$UNIT_DIR/glidekm-client.service" <<EOF
[Unit]
Description=glidekm client (receive mouse/keyboard from PC)
After=network-online.target

[Service]
ExecStart=$BIN_DIR/glidekm-client
Restart=always
RestartSec=2

[Install]
WantedBy=default.target
EOF
systemctl --user daemon-reload
systemctl --user enable --now glidekm-client.service
systemctl --user restart glidekm-client.service
sleep 1
systemctl --user --no-pager status glidekm-client.service | head -n 5 || true

IP="$( (ip -4 route get 192.0.2.1 2>/dev/null || true) | sed -n 's/.* src \([0-9.]*\).*/\1/p')"
echo
echo "Done. On the PC set:  client = ${IP:-<this-deck-ip>}:4242"
echo "Logs: journalctl --user -u glidekm-client -f"
