# GPU-primary selection — spec v2 (post adversarial review)

Reviewed 2026-06-12 by a 3-lens adversarial panel (startup safety, FSM architecture,
hardware genericity): 18 findings, 16 accepted verdicts folded in below.

## Problem

Multi-GPU machines (Framework 16: iGPU Radeon 780M `pci-0000:c4:00.0`, dGPU RX 7700S
`pci-0000:03:00.0`) need the right GPU as Hyprland's primary renderer. Primary is set
via `AQ_DRM_DEVICES` (colon-separated, first = primary) which **must be in Hyprland's
environment before launch and cannot change at runtime** — a relog is required.
Wayland clients (Chromium etc.) follow the compositor's main DRM device automatically.

Two halves:
1. **Session start (pre-compositor)**: uwsm's `~/.config/uwsm/env-hyprland` invokes
   `hyprstate gpu select` and exports the result. No Hyprland, no daemon, no D-Bus.
2. **Runtime (daemon)**: detect drift between what the session uses and what current
   topology/power-profile would choose; notify (never auto-relog).

Dotfiles are chezmoi-shared across machines with different hardware (incl.
single-GPU) — selection must be pure hardware detection, zero machine conditionals,
silent no-op where it doesn't apply.

## CLI surface

```
hyprstate gpu select   # compute; write state file; print device list (or nothing)
hyprstate gpu check    # compute; print; NO state-file write
hyprstate gpu status   # human-readable: state file vs live check, sync verdict
```

**Output format — `/dev/dri/cardN` device nodes, NOT by-path.** `AQ_DRM_DEVICES`
is colon-separated and PCI by-path names contain colons (`pci-0000:03:00.0-card`),
so aquamarine (verified on 0.9.5) shatters a by-path value on every `:` →
"Failed to canonicalize path … Found no gpus to use" → backend fails → no GUI.
We SELECT by the stable PCI by-path (cardN renumbers across boots) but EMIT the
resolved cardN node, recomputed fresh every login so renumbering is harmless.

`select`/`check` print a colon-separated `/dev/dri/cardN` list (primary first)
on stdout, or **nothing** when unmanaged. Exit 0 in all those cases. The `gpu`
subcommand path never configures logging to stdout (daemon's `stream=sys.stdout`
convention is forbidden here — stderr only); the device list is emitted exactly once,
as the final statement, after all validation and the state-file attempt. No
hyprctl/D-Bus (pre-compositor); sysfs only; imports stay light.

## Topology snapshot (sysfs only)

A `/dev/dri/by-path/*-card` entry is a **GPU candidate** only if:
(a) its name has no `-usb-`/`-usbv2-` segment (excludes udl/DisplayLink),
(b) resolved `/sys/class/drm/cardN/device/class` exists and starts with `0x03`
    (PCI display controller),
(c) deduplicated by resolved `cardN`.
The candidate filter applies to the `< 2` bail count, not just classification.

Per candidate: `boot_vga` (missing → 0), `mem_info_vram_total` (missing → 0;
Intel/nouveau lack it), connected connectors from `/sys/class/drm/cardN-*/status`
(eDP classified by connector name; `*-Writeback-*` skipped).

**Non-PCI guard**: after building the candidate set, scan the rejected/non-PCI
`*-card` entries; if any has a connected connector → unmanaged bail
(`reason: non-pci-display-present`). Listing platform/evdi devices in
`AQ_DRM_DEVICES` is untested; reproducing today's open-all-GPUs behavior is the
safe choice. Lost runtime-PM saving on DisplayLink machines is the accepted cost.

**Classification**: compute both signals — `boot_vga==1` and smallest VRAM — and
require agreement on which card is integrated; disagreement (e.g. muxed laptop with
boot_vga on the discrete) or VRAM tie without boot_vga → unmanaged. (Intel agrees
trivially via missing-vram=0; FW16 agrees today; a future BIOS mux flip produces
disagreement → safe bail, not inversion.)
Discrete candidates sorted by (external display count desc, VRAM desc); best = [0].

## Mode resolution (precedence)

1. Override file `~/.config/hypr/gpu-select`, first word: `igpu|dgpu|off|auto`
   (unknown content → ignore file, log to stderr). Runtime user state, NOT
   chezmoi-delivered.
2. Profile breadcrumb `~/.config/hypr/gpu-profile` (see Profile directive):
   `igpu|dgpu` honored, `auto`/missing/empty → fall through.
3. `/sys/firmware/acpi/platform_profile`: `low-power` or `quiet` → `igpu`;
   `performance` → `dgpu`; `balanced`, `balanced-performance`, `cool`, `custom`,
   missing, unknown → `auto` (deliberate; exhaustive against the kernel ABI).
4. Default `auto`.

## Selection (pure function of snapshot + mode)

- `< 2` GPU candidates → unmanaged (print nothing).
- `off` → unmanaged.
- `auto`: best discrete has a connected display (external or eDP) → discrete
  primary; else iGPU primary. Either way, **all non-primary candidates (including
  display-less discretes) are listed** as trailing secondaries. aquamarine
  (≥ PR#239 / 0.10) deinitializes a secondary renderer with no enabled outputs, so
  amdgpu runtime PM still suspends the idle dGPU — *that* is the power saving — while
  the card stays available for live hotplug (re-inits on connect) and PRIME offload.
  (Older aquamarine kept listed secondaries powered; this mode assumes ≥ PR#239.)
- `igpu`: integrated primary; discretes listed only if they have a connected
  display, omitted otherwise.
- `dgpu`: best discrete primary and always listed (performance mode keeps it awake
  deliberately); integrated always listed.
- Integrated is always included (eDP / future hotplug headroom).
- **Usable-output invariant**: never print a list under which the snapshot contains
  no usable output (a connected external on a listed card, or eDP with lid open).
- **Transient-disconnect settle**: `select` reads lid state from
  `/proc/acpi/button/lid/*/state`. If lid closed AND zero connected externals
  anywhere: one settle retry (re-snapshot after 500 ms); still zero → unmanaged,
  `reason: bailed-transient`. (Covers docked cold boot where DP links aren't up at
  early-login sysfs read; without this, omitting the dock's GPU would leave the
  lid-closed session with no usable output and the lid FSM would suspend-loop.)
- **All-or-nothing validation**: if ANY computed path fails `os.path.exists` at
  print time → entire selection unmanaged (`reason: validation-failed`). Dropping
  individual paths is forbidden (could silently violate the invariants above).

## State file — contract with the daemon

`$XDG_RUNTIME_DIR/hypr-gpu-primary.json`, atomic write (tmp + rename), best-effort
(write failure must not affect stdout). Schema v1:

```json
{
  "version": 1,
  "mode": "auto",
  "reason": "dgpu-has-display",
  "primary": "/dev/dri/card1",
  "devices": ["...03:00.0-card", "...c4:00.0-card"],
  "omitted": [],
  "snapshot": {"pci-0000:03:00.0": {"type": "discrete", "boot_vga": 0,
                "vram": 8573157376, "external": 1, "edp": 0}, "...": {}}
}
```

`reason` enum: `no-multi-gpu | dgpu-has-display | dgpu-idle-listed | override-igpu |
override-dgpu | profile-igpu | profile-dgpu | platform-igpu | platform-dgpu |
bailed-transient | validation-failed | non-pci-display-present | ambiguous-integrated`.
State file = *intent* record (mode/reason, for `gpu status`); ground truth for
*actual* is Hyprland's environ (below). No timestamp (runtime dir is per-boot;
staleness neutralized by environ-as-actual).

## Profile directive + breadcrumb

New `#@ gpu = auto|igpu|dgpu` (default `auto`) parsed in `_parse_profile` alongside
`#@ edp`; invalid value → profile skipped (consistent with `edp` validation).

The directive cannot reach next login directly (profiles match against *live*
monitors, which don't exist pre-compositor), and reading `.active.conf` at select
time creates a relog loop when topology changed since the symlink was set. So:
**breadcrumb file** `~/.config/hypr/gpu-profile` (runtime user state, NOT
chezmoi-delivered) — on every profile reconcile the daemon writes the matched
profile's `#@ gpu` value, or clears the file on no-match. `gpu select` reads it at
precedence step 2. Because the daemon updates the breadcrumb *before* any drift
notification fires, "relog to apply" is always satisfiable by one relog: desired and
next-login select compute from identical inputs. Topology change after relog may
produce at most one further notification, never a loop.

Drift check overlay: always the **fresh** `select_profile` result (None → overlay
dropped, falls through to platform_profile/auto), never `ctx.current_profile`.

## Daemon integration

- `EventKind` gains `PLATFORM_PROFILE_CHANGED` and `GPU_OVERRIDE_CHANGED` (routed
  identically). A poller task — sibling of `inhibitor_poller`, own `last_*` locals —
  polls `platform_profile` content and the override file's first word, queues on
  change. The reconciler stays event-free (its contract is silent ctx repair; the
  existing AC sysfs fallback only mutates ctx, verified line 809-811).
- **Actual**: `ctx.gpu_actual` resolved *lazily* on first drift check with retry
  (mirroring `hypr_socket_reader`'s 2 s retry): PID = first line of
  `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/hyprland.lock`; validate
  `/proc/<pid>/comm == "Hyprland"`; parse `/proc/<pid>/environ` split on `\0`;
  extract `AQ_DRM_DEVICES`. pgrep is forbidden (nested Hyprland is a known use
  pattern). Unset var after confirmed-running → unmanaged session (drift checking
  off). Re-resolve if cached PID vanishes.
- **Desired**: pure `gpu_desired(snapshot, mode) -> (devices|None, reason)` shared
  by CLI and daemon (Layer-3 style, like `select_profile`). The dispatcher computes
  desired, compares, debounces, and calls a narrow `fx.notify_gpu_drift(desired,
  trigger)` whose sole job is notify-send (Layer-1 style, like `apply_profile`).
- Call sites (exact): inside the `MONITORS_CHANGED` branch after the
  apply-profile/no-match decision, before its `continue`; and in the fall-through
  path gated on `ev.kind in (AC_PLUGGED, AC_UNPLUGGED, PLATFORM_PROFILE_CHANGED,
  GPU_OVERRIDE_CHANGED)`. NOT routed through `RECONCILE` (early `continue` skips it).
- If state file reads `bailed-transient`, the first `MONITORS_CHANGED` drift check
  must notify when a dGPU-port display has appeared.
- Debounce: notify once per distinct desired-list (`ctx.gpu_last_notified`), BUT
  (1) reset the key whenever desired == actual (re-dock after sync must re-notify),
  (2) 60 s monotonic minimum interval between notifications.
- Battery state may flavor notification copy; it never changes the decision.
  Never auto-relog.
- `hyprstate status` adds: `gpu: primary=<...> mode=<...> reason=<...> — in sync` /
  `— MISMATCH (relog to apply: <desired>)` / `— unmanaged`.

## uwsm hookup (dotfiles side, separate chezmoi change)

```sh
# Dynamic GPU selection (multi-GPU machines only). hyprstate prints a
# colon-separated DRM device list (primary first) or nothing when unmanaged.
# Charset + shape guards: anything unexpected leaves AQ_DRM_DEVICES unset =
# compositor defaults = pre-feature behavior. Override:
#   echo igpu|dgpu|off > ~/.config/hypr/gpu-select
_gpu_sel=/usr/local/bin/hyprstate
if [ -x "$_gpu_sel" ]; then
    _aq=$("$_gpu_sel" gpu select 2>/dev/null) || _aq=""
    case "$_aq" in *[!/:.A-Za-z0-9_-]*) _aq="" ;; esac   # reject newlines/garbage
    case "$_aq" in
        /dev/dri/card*) export AQ_DRM_DEVICES="$_aq" ;;
    esac
fi
unset _gpu_sel _aq
```

Failure analysis: missing binary → skipped; old binary without the subcommand →
argparse exits 2 with stderr usage, stdout empty → `_aq=""` → unset; crash after
partial stdout → charset/shape guards reject; every failure path leaves
`AQ_DRM_DEVICES` unset. ~100 ms Python in the login path accepted.

## Known accepted limitations

- `auto` assumes aquamarine ≥ PR#239 (≥ 0.10), which deinitializes idle secondary
  renderers so a listed-but-display-less dGPU suspends. On older aquamarine a listed
  idle secondary stays powered (battery drain) — use `igpu` mode there. (Verified
  stack: Hyprland 0.54 linking libaquamarine 0.11.0.)
- `igpu` mode still omits the display-less dGPU entirely, so a display hotplugged
  into its port mid-session is dead output until relog (daemon notifies). In `auto`
  the dGPU is listed, so aquamarine lights up the hotplugged display live.
- Mid-session platform_profile / override changes only notify; selection is
  login-time.
- DisplayLink-equipped machines are always unmanaged (non-PCI guard) — no runtime-PM
  saving there.
