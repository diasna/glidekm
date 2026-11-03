#!/usr/bin/env bash
# glidekm client installer for SteamOS (Desktop Mode, Konsole) or any Linux.
# Installs to ~/.local/bin, a systemd *user* service (runs in Game Mode too),
# a "glidekm Settings" app-menu entry, and, if needed, a udev rule giving the
# logged-in user access to /dev/uinput.
#
#   ./install.sh              install / update
#   ./install.sh --uninstall  remove everything
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
BIN_DIR="$HOME/.local/bin"
CONF_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/glidekm"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
APP_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
ICON_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/scalable/apps"
UDEV_RULE=/etc/udev/rules.d/60-glidekm-uinput.rules

if [[ "${1:-}" == "--uninstall" ]]; then
  systemctl --user disable --now glidekm-client.service 2>/dev/null || true
  rm -f "$UNIT_DIR/glidekm-client.service" "$BIN_DIR/glidekm-client" "$BIN_DIR/glidekm-settings" \
    "$APP_DIR/glidekm-settings.desktop" "$ICON_DIR/glidekm.svg"
  systemctl --user daemon-reload || true
  echo "Removed binaries, service and menu entry. Config kept in $CONF_DIR."
  [[ -f $UDEV_RULE ]] && echo "To remove the udev rule: sudo rm $UDEV_RULE"
  exit 0
fi

mkdir -p "$BIN_DIR" "$CONF_DIR" "$UNIT_DIR" "$APP_DIR" "$ICON_DIR"
install -m 755 "$HERE/glidekm-client" "$BIN_DIR/glidekm-client"
install -m 755 "$HERE/glidekm-settings" "$BIN_DIR/glidekm-settings"
echo "Installed glidekm-client and glidekm-settings to $BIN_DIR"

# App-menu entry "glidekm Settings" (KDE menu: Settings / Utilities).
install -m 644 "$HERE/glidekm.svg" "$ICON_DIR/glidekm.svg"
cat > "$APP_DIR/glidekm-settings.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=glidekm Settings
GenericName=Mouse & keyboard sharing
Comment=Use your PC's mouse and keyboard on this Deck
Exec=$BIN_DIR/glidekm-settings
StartupWMClass=glidekm-settings
Icon=$ICON_DIR/glidekm.svg
Terminal=false
Categories=Settings;Utility;
Keywords=mouse;keyboard;share;pc;
EOF
if command -v update-desktop-database >/dev/null; then
  update-desktop-database "$APP_DIR" 2>/dev/null || true
fi
echo "Added menu entry: glidekm Settings"

# /dev/uinput access. Steam's own udev rules usually grant it to the active
# user already; otherwise add a tiny rule (needs sudo; on a Deck, set a
# password first with `passwd`). glidekm Settings shows this too.
if [[ ! -w /dev/uinput ]]; then
  echo "No write access to /dev/uinput - installing udev rule (sudo)."
  if echo 'KERNEL=="uinput", SUBSYSTEM=="misc", TAG+="uaccess", OPTIONS+="static_node=uinput"' |
    sudo tee "$UDEV_RULE" >/dev/null; then
    sudo modprobe uinput || true
    sudo udevadm control --reload-rules
    sudo udevadm trigger --name-match=uinput || true
    sleep 1
  fi
  if [[ ! -w /dev/uinput ]]; then
    echo "WARNING: /dev/uinput still not writable. Log out and back in (or reboot)."
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
systemctl --user enable --now glidekm-client.service ||
  echo "WARNING: could not enable the service (systemd user session missing?)"
systemctl --user restart glidekm-client.service || true
sleep 1

echo
echo "Done. Opening glidekm Settings (also in the app menu) to set the passphrase..."
if [[ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]]; then
  nohup "$BIN_DIR/glidekm-settings" >/dev/null 2>&1 &
else
  echo "(no desktop session: open 'glidekm Settings' from the app menu later)"
fi
echo "Logs: journalctl --user -u glidekm-client -f"
