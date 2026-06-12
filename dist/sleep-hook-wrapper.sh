#!/bin/bash
# Installed at /usr/lib/systemd/system-sleep/hyprstate (package-owned).
# systemd-suspend.service runs every executable in that directory as root
# with: <pre|post> <suspend|hibernate|hybrid-sleep|suspend-then-hibernate>.
exec /usr/bin/hyprstate sleep-hook "$1"
