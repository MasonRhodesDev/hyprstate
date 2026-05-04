#!/bin/bash
# Wrapper installed at /usr/lib/systemd/system-sleep/hyprstate.
# systemd-suspend.service runs every executable in that directory with
# arguments: <pre|post> <suspend|hibernate|hybrid-sleep|suspend-then-hibernate>.
# We only care about pre/post; pass through.
exec /usr/local/bin/hyprstate sleep-hook "$1"
