#!/bin/bash
# Idempotent installer for hyprstate.
# Symlinks the binary + sleep hook into system paths, drops the systemd user unit.
# Migrates from predecessor names (hypr-power, hypr-fsm) if found.
set -euo pipefail

SRC="$(cd "$(dirname "$0")" && pwd)"
BIN_TARGET="/usr/local/bin/hyprstate"
HOOK_TARGET="/usr/lib/systemd/system-sleep/hyprstate"
USER_UNIT_DIR="$HOME/.config/systemd/user"

echo "Source: $SRC"

# ---- 1. system-level: binary symlink + sleep hook + udev rule (one sudo prompt) ----
chmod +x "$SRC/hyprstate.py" "$SRC/system-sleep-hook.sh"
sudo ln -sfn "$SRC/hyprstate.py" "$BIN_TARGET"
sudo ln -sfn "$SRC/system-sleep-hook.sh" "$HOOK_TARGET"
sudo install -m 644 "$SRC/60-hyprstate-usb-wake.rules" /etc/udev/rules.d/60-hyprstate-usb-wake.rules
sudo udevadm control --reload-rules
echo "  -> $BIN_TARGET"
echo "  -> $HOOK_TARGET"
echo "  -> /etc/udev/rules.d/60-hyprstate-usb-wake.rules"

# ---- 2. user-level: systemd unit ----
mkdir -p "$USER_UNIT_DIR"
cp "$SRC/hyprstate.service" "$USER_UNIT_DIR/hyprstate.service"
systemctl --user daemon-reload
echo "  -> $USER_UNIT_DIR/hyprstate.service"

# ---- 3. swap from predecessor services (hypr-power, hypr-fsm) ----
for old in hypr-power.service hypr-fsm.service; do
    if systemctl --user is-enabled "$old" &>/dev/null \
       || systemctl --user is-active  "$old" &>/dev/null; then
        echo "Disabling predecessor $old"
        systemctl --user disable --now "$old" || true
    fi
    rm -f "$USER_UNIT_DIR/$old"
done
rm -f "$HOME/.config/hypr/configs/lid-fsm.py"

# ---- 4. clean up legacy USB-wake stack (now superseded) ----
# The previous setup had a monitor timer (status logger), an unwired
# PrepareForSleep handler service, an orphan sleep-hook script, a
# user-home udev rule (dead — udev only reads /etc and /usr), and
# an /etc-installed udev rule. hyprstate replaces all of them.
for u in usb-wake-monitor.timer usb-wake-monitor.service usb-wake-sleep-handler.service; do
    if systemctl --user is-enabled "$u" &>/dev/null || systemctl --user is-active "$u" &>/dev/null; then
        echo "Disabling legacy $u"
        systemctl --user disable --now "$u" 2>/dev/null || true
    fi
done
rm -f "$USER_UNIT_DIR/usb-wake-monitor.service"
rm -f "$USER_UNIT_DIR/usb-wake-monitor.timer"
rm -f "$USER_UNIT_DIR/usb-wake-sleep-handler.service"
rm -f "$HOME/.local/bin/usb-wake-monitor.sh"
rm -f "$HOME/.local/share/systemd-sleep-hooks/usb-wake"
rmdir "$HOME/.local/share/systemd-sleep-hooks" 2>/dev/null || true
rm -f "$HOME/.config/udev/rules.d/60-usb-wake.rules"
rmdir "$HOME/.config/udev/rules.d" "$HOME/.config/udev" 2>/dev/null || true

# Remove old-named system symlinks/files left by previous install runs.
sudo rm -f /etc/udev/rules.d/60-usb-wake.rules
sudo rm -f /etc/udev/rules.d/60-hypr-power-usb-wake.rules
sudo rm -f /usr/local/bin/hypr-power
sudo rm -f /usr/lib/systemd/system-sleep/hypr-power
sudo udevadm control --reload-rules

# ---- 5. enable + start ----
systemctl --user enable --now hyprstate.service
echo
echo "Installed. Tail logs: journalctl --user -u hyprstate.service -f"
