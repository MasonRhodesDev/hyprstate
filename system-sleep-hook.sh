#!/bin/bash
# Installed (as a root-owned COPY, never a symlink) at
# /usr/lib/systemd/system-sleep/hyprstate. systemd-suspend.service runs every
# executable in that directory as root with arguments:
# <pre|post> <suspend|hibernate|hybrid-sleep|suspend-then-hibernate>.
#
# Privilege boundary (POWER_SPEC V3): root must never execute user-writable
# code, so this execs the root-owned libexec copy — NOT the user-writable dev
# symlink at /usr/local/bin/hyprstate. Updating the hook = rerun install.sh.
exec /usr/local/libexec/hyprstate sleep-hook "$1"
