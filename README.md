# hyprstate

Single-process session state machine for Hyprland on Framework 16. Owns lid, monitor profiles, eDP-2, lock, suspend, and USB-wake.

## What it owns

- **Lid switch.** Holds a logind `handle-lid-switch:block` inhibitor so logind doesn't suspend on lid close. The FSM decides instead.
- **eDP-2 enable/disable.** Disabled when lid closed, re-enabled (via `hyprctl reload`) when lid opens. Hard invariant — re-asserted by a 5s reconciler and on `configreloaded` events.
- **Monitor profiles.** Auto-applies a profile based on the set of currently-connected monitors. Profiles live as `.conf` snippets in `~/.config/hypr/profiles/` with `#@` directive comments for match signature, hooks, and eDP policy.
- **Suspend grace.** Lid close → 30s window before suspending. Cancellable by lid reopen, monitor hotplug, or new idle inhibitor.
- **Idle-inhibitor awareness.** If an inhibitor is already active at lid close, media is paused (`playerctl --all-players pause`) and the countdown is deferred until the inhibitor releases.
- **Lock-before-suspend.** Calls `Session.Lock()` before `Manager.Suspend()`, waits up to 2s for `LockedHint=true`.
- **DPMS-off when locked + inhibitor.** With an active screen (`LID_OPEN` or `DOCKED`) and the session locked while an inhibitor is held, screens DPMS-off after 30s. Reverses on unlock or inhibitor release.
- **Input-device wake.** A pre/post systemd-sleep hook keeps `/sys/.../power/wakeup` enabled on USB hubs, the ZSA Voyager keyboard, and the Logitech Lightspeed mouse.

## Layout

```
hyprstate.py             single-file program with all subcommands
hyprstate.service        systemd --user unit
system-sleep-hook.sh     wrapper invoked by /usr/lib/systemd/system-sleep/
install.sh               idempotent installer
```

## Subcommands

```
hyprstate daemon              # run the FSM (systemd --user)
hyprstate sleep-hook pre|post # invoked by systemd-suspend (root)
hyprstate install             # symlink + drop systemd unit
hyprstate uninstall           # reverse install
hyprstate status              # systemctl + journalctl summary
hyprstate profile list        # list known profiles
hyprstate profile current     # show currently-applied profile
hyprstate profile switch NAME # force-apply a profile
```

## Install

```
./install.sh        # one sudo prompt, symlinks system bits
```

The installer migrates from predecessor names (`hypr-power.service`, `hypr-fsm.service`) and removes the orphan `~/.local/share/systemd-sleep-hooks/usb-wake`.

## Debug

```
journalctl --user -u hyprstate.service -f       # daemon log
sudo tail -f /var/log/hyprstate-sleep.log       # sleep hook log
```

## Dependencies

System: `hyprctl`, `playerctl`, `hyprlock` (via hypridle's `lock_cmd`), `pgrep`, `hypridle` (catches the logind Lock signal and runs hyprlock).
Python: `dbus-next` (Fedora `python3-dbus-next`).
