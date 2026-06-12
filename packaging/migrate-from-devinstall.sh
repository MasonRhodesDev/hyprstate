#!/bin/bash
# One-shot migration from the git-symlink dev install (and the interim v2
# drop-in overrides) to the packaged install. Run once per machine, then
# install the package (dnf copr / pacman). Idempotent; safe to re-run.
#
# Deliberately NOT in the RPM %post: half this state is user-level (root
# scriptlets can't reach it), and fresh machines never need it.
set -uo pipefail

echo "=== hyprstate: migrate from dev install ==="

# 1. Stop + disable services (removes enable symlinks pointing at the old
#    unit files).
systemctl --user disable --now hyprstate.service 2>/dev/null
sudo systemctl disable --now hyprstate-powerd.service 2>/dev/null

# 2. Interim v2 cutover artifacts (drop-ins + root-owned binary copy).
sudo rm -f /etc/systemd/system/hyprstate-powerd.service.d/v2-rust.conf
sudo rmdir /etc/systemd/system/hyprstate-powerd.service.d 2>/dev/null
rm -f "$HOME/.config/systemd/user/hyprstate.service.d/v2-rust.conf"
rmdir "$HOME/.config/systemd/user/hyprstate.service.d" 2>/dev/null
sudo rm -f /usr/local/libexec/hyprstate-v2

# 3. Dev-install artifacts. The /etc systemd unit would SHADOW the packaged
#    /usr/lib unit; the /etc/udev rule (same filename) would fully OVERRIDE
#    the packaged /usr/lib rule; the /etc dbus policy would duplicate the
#    packaged /usr/share one.
sudo rm -f /usr/local/bin/hyprstate
sudo rm -f /usr/local/libexec/hyprstate
sudo rm -f /usr/lib/systemd/system-sleep/hyprstate
sudo rm -f /etc/systemd/system/hyprstate-powerd.service
sudo rm -f /etc/dbus-1/system.d/org.hyprstate.Power1.conf
sudo rm -f /usr/share/dbus-1/system-services/org.hyprstate.Power1.service
sudo rm -f /etc/udev/rules.d/60-hyprstate-usb-wake.rules
rm -f "$HOME/.config/systemd/user/hyprstate.service"

# 4. Predecessor stacks (hypr-power / hypr-fsm / usb-wake-monitor) for
#    machines that never ran the newer dev installer.
for u in hypr-power.service hypr-fsm.service usb-wake-monitor.timer \
         usb-wake-monitor.service usb-wake-sleep-handler.service; do
    systemctl --user disable --now "$u" 2>/dev/null
    rm -f "$HOME/.config/systemd/user/$u"
done
rm -f "$HOME/.local/bin/usb-wake-monitor.sh" \
      "$HOME/.local/share/systemd-sleep-hooks/usb-wake" \
      "$HOME/.config/udev/rules.d/60-usb-wake.rules"
rmdir "$HOME/.local/share/systemd-sleep-hooks" \
      "$HOME/.config/udev/rules.d" "$HOME/.config/udev" 2>/dev/null
sudo rm -f /etc/udev/rules.d/60-usb-wake.rules \
           /etc/udev/rules.d/60-hypr-power-usb-wake.rules \
           /usr/local/bin/hypr-power \
           /usr/lib/systemd/system-sleep/hypr-power

# 5. Reload everything that cached old definitions.
sudo systemctl daemon-reload
systemctl --user daemon-reload
sudo udevadm control --reload
sudo systemctl reload dbus-broker.service 2>/dev/null

# 6. Boot-race guard: the packaged unit Conflicts= with p-p-d at runtime,
#    but two boot-enabled conflicting units race at startup.
if systemctl is-enabled power-profiles-daemon.service >/dev/null 2>&1; then
    echo "Disabling power-profiles-daemon (hyprstate powerd owns platform_profile)"
    sudo systemctl disable --now power-profiles-daemon.service
fi

echo
echo "Migration done. Now install the package:"
echo "  Fedora: sudo dnf copr enable <fas>/hyprstate && sudo dnf install hyprstate"
echo "  Arch:   paru -S hyprstate   (or makepkg from packaging/)"
echo "Then: sudo systemctl start hyprstate-powerd && systemctl --user start hyprstate"
