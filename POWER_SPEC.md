# Power management — spec v2 (post adversarial review)

Reviewed 2026-06-12 by a 3-lens adversarial panel (root-daemon security, FSM/races,
hardware genericity): 24 findings → 16 merged verdicts, all accepted, folded in.

hyprstate owns power management end-to-end: **policy** in the user daemon (pure map
over base states), **mechanism** in `hyprstate powerd` (root, systemd system
service, narrow D-Bus interface). No ppd/tuned/TLP (`Conflicts=` guards). Chezmoi
delivers config + waybar. Power profile is the single source that feeds GPU
selection (GPU_SPEC.md); hyprstate is the sole intended writer of
`platform_profile`.

## Component 1 — `hyprstate powerd` (root, mechanism only)

### Privilege boundary (V3)

install.sh installs a **root-owned copy**: `sudo install -m 755 -o root -g root
hyprstate.py /usr/local/libexec/hyprstate`; the system unit's ExecStart uses that
copy. The dev symlink `/usr/local/bin/hyprstate` (user-writable target) remains
for the *user* daemon + CLI only. Root never executes user-writable code; powerd
changes require re-running install.sh (already the documented update flow).

### D-Bus interface

```
Bus: system. Name: org.hyprstate.Power1. Path: /org/hyprstate/Power1.
ApplyProfile(s) -> a{ss}   # profile ∈ {power-saver,balanced,performance}, else D-Bus error
SetDgpuAwake(b) -> a{ss}   # pin (true) / release (false) discrete-GPU runtime PM
GetProfile() -> s          # persisted active profile
GetKnobs() -> a{ss}        # read-only live snapshot (incl. runtime_pm:<pci>)
signal ProfileApplied(s);  property ActiveProfile(s, emits-changes)
```

- **Success semantics (V17a)**: ApplyProfile success = the call completed.
  Per-row results (`written|unchanged|skipped-missing|skipped-suspended|
  skipped-unsupported|skipped-ambiguous|error:<msg>`) are informational; an
  all-skipped apply is still success (VM/desktop case, V20b).
- **Coalescing (V14)**: calls arriving while an apply is in flight update a
  latest-request slot; superseded waiters return `{"coalesced":
  "superseded-by:<profile>"}`; only first and latest apply. Per-row work is
  read-before-write idempotent.
- **Discrete-GPU runtime-PM pin (`SetDgpuAwake`)**: writes `power/control` =
  `on` (pin, block D3cold autosuspend) / `auto` (release, kernel default) to
  every discrete (non-integrated) card; same discovery guards as the dpm rows
  (≥2 cards + unambiguous integrated, else `skipped-ambiguous`). The
  dgpu-vs-other *decision* is policy and lives in the user daemon
  (`pure::gpu::dgpu_runtime_pm_pinned` — only `dgpu` mode pins); powerd is pure
  mechanism. Rationale: on Framework 16 a dGPU D3cold resume can leave the
  display engine wedged (`amdgpu: [drm] Cannot find any crtc or sizes`) until a
  cold boot, and dgpu mode keeps that card the active renderer, so it must
  never autosuspend. Unlike the dpm knob, `power/control` is the autosuspend
  gate itself — reading/writing it never wakes a suspended card, so no
  runtime_status guard. Persisted to `/var/lib/hyprstate/dgpu-pin` (`on|auto`,
  tmp+rename atomic, missing → `auto`); re-applied at startup and on resume.
- **Persisted profile (V4)**: `/var/lib/hyprstate/profile`, tmp+rename atomic;
  read validated against the profile whitelist; invalid/missing → `balanced` +
  warning; the knob matrix is never indexed with an unvalidated string.
- **Resume**: own PrepareForSleep(false) subscription → re-apply persisted
  profile through the same idempotent path (V20a: not an amplification vector),
  then re-apply the persisted dgpu pin (a D3cold resume across s2idle can reset
  `power/control`, and the pin is exactly what keeps dgpu mode wedge-proof).

### Bus policy — verbatim (V2)

`/etc/dbus-1/system.d/org.hyprstate.Power1.conf`:

```xml
<busconfig>
  <policy user="root"><allow own="org.hyprstate.Power1"/></policy>
  <policy group="wheel">
    <allow send_destination="org.hyprstate.Power1"/>
  </policy>
</busconfig>
```

The own-allow MUST appear only in the root policy (a wheel/default own-allow
lets any local user squat the name pre-boot; Type=dbus would even report the
unit as started). install.sh asserts `systemctl is-active hyprstate-powerd`
post-install so a malformed policy fails loudly. Additionally ship
`/usr/share/dbus-1/system-services/org.hyprstate.Power1.service` with
`SystemdService=hyprstate-powerd.service` (V11c) — bus activation closes the
boot race for early callers.

### Knob matrix

| Knob | power-saver | balanced | performance |
|---|---|---|---|
| platform_profile — value resolved through a **fallback chain** validated against `_choices`: power-saver→[low-power, quiet]; none present → skipped-unsupported (V7) | low-power | balanced | performance |
| `policy*/scaling_governor` — **only when EPP-capable** (`energy_performance_preference` exists or scaling_driver ∈ {amd-pstate-epp, intel_pstate}); value validated against `scaling_available_governors`; else skipped-unsupported (V12 — on acpi-cpufreq `powersave` PINS MIN FREQ; on schedutil kernels the values don't exist) | powersave | powersave | performance |
| `policy*/energy_performance_preference` — written AFTER governor; EBUSY under performance governor → skipped-unsupported | power | balance_performance | (implied) |
| `cpufreq/boost` — probe first; if absent probe `intel_pstate/no_turbo` (inverted: 1/0/0, V15); EPERM (BIOS-locked) → skipped-unsupported | 0 | 1 | 1 |
| discrete amdgpu `power_dpm_force_performance_level` — **check `power/runtime_status` FIRST; suspended → skipped-suspended, never open the knob** (reading it wakes the card) | low | auto | auto |
| integrated amdgpu dpm level | auto | auto | auto |
| `pcie_aspm/parameters/policy` — value validated against the file's own bracket-annotated option list; one same-value write probe at startup, EPERM → skipped-unsupported permanently (V17b, BIOS-disabled ASPM) | powersupersave | default | default |

- **GPU discovery guards (V13)**: reuse `gpu_snapshot()`; `_integrated_card()`
  called only when ≥2 cards (it ValueErrors on empty input); 0 cards → both GPU
  rows skipped-missing; 1 card or ambiguous → skipped-ambiguous (single-card
  desktops must not have their dGPU misclassified as integrated and clamped).
- **Per-row exception isolation (V13)**: any row exception → `error:<msg>` in
  the results map; never a failed ApplyProfile.
- Excluded (unchanged from v1): pp_power_profile_mode, mem_sleep, wifi
  powersave, charge thresholds, keyboard backlight.

### Unit

Type=dbus, BusName=org.hyprstate.Power1,
ExecStart=/usr/local/libexec/hyprstate powerd, StateDirectory=hyprstate,
ProtectSystem=strict, **ProtectHome=yes** (viable now — binary no longer in
$HOME, V3), **no ProtectKernelTunables** (would remount /sys ro),
NoNewPrivileges, PrivateTmp, PrivateNetwork,
RestrictAddressFamilies=AF_UNIX, SystemCallFilter=@system-service,
CapabilityBoundingSet= (empty), Conflicts=power-profiles-daemon.service
tuned.service tlp.service, WantedBy=multi-user.target.

## Component 2 — policy in the user daemon

### Base states & config

`docked-ac` (on_ac_settled ∧ ext_mon_count ≥ 1) | `ac` | `battery` |
`battery-low` (enter ≤ threshold, exit ≥ threshold+3). The AC axis is decided
by `on_ac_settled` alone — desktops never see an unplug, so no battery →
permanently `ac`/`docked-ac` falls out without a special case (V10), and a
laptop with UPower down still reaches battery profiles via the V8 reconciler
repair. `battery_percent` gates only the low-battery machinery; it must NOT
gate the axis (that would pin a UPower-down laptop to AC profiles, defeating
V8).

`~/.config/hypr/power.conf` (chezmoi-delivered), parsed by a **dedicated
`_POWER_DIRECTIVE_RE = ^#@\s*([a-z][a-z-]*)\s*=\s*(.+?)\s*$`** (V1 — the shared
profile regex has no hyphens and must not gain them; loader logs parsed keys and
unrecognized `#@` lines):

```
#@ docked-ac = balanced
#@ ac = balanced
#@ battery = power-saver
#@ battery-low = power-saver
#@ battery-low-percent = 15
```

Missing file/keys → defaults above; values validated ∈ profiles. `battery*`
keys inert on desktops — deliberate, not templated (V20c).

### Inputs (V5, V8, V10)

- **AC**: raw AC_PLUGGED/UNPLUGGED events start/cancel a 5 s debounce task that
  enqueues a new `POWER_AC_SETTLED` event; `ctx.on_ac_settled` updates only when
  it's consumed. The power gate listens to POWER_AC_SETTLED, NOT raw AC events.
  Opposite flips cancel-and-restart the task.
- **Reconciler exception (V8)**: when the reconciler repairs `on_ac` or
  `ext_mon_count` drift it enqueues POWER_AC_SETTLED — the one documented
  exception to its event-free contract; the event feeds only power policy.
  Covers boot-on-battery with UPower down.
- **Battery (V10)**: `setup_upower_watcher` does an initial DisplayDevice
  GetAll (Percentage, IsPresent, Type) before the startup policy evaluation.
  IsPresent=false or Type ∉ {battery, ups} → `battery_percent=None`,
  battery machinery disabled, status prints "no battery". Initial low_battery
  = entry rule on first sample. Then Percentage PropertiesChanged; ctx eager,
  `BATTERY_LOW_CHANGED` enqueued only on hysteresis flips.
- **Monitor count robustness (V9c)**: `_hyprctl_ext_monitor_count` returns the
  previous count on hyprctl failure (transient hyprctl errors must not derive a
  fake base-state change).

### Override semantics (V6, V9, V19)

- Override file `~/.config/hypr/power-override` carries **profile only**; the
  daemon stamps `power_override_base` itself when it first ingests the override
  (the CLI cannot know the hysteresis-adjusted base).
- **Expiry (V9)**: only on the AC axis flipping (ac↔battery) or battery-low
  *entry*; docked-ac↔ac never expires an override (a display blink must not
  silently delete explicit user intent). Expiry deletes the file, updates ctx
  **synchronously**, and notify-sends.
- **Idempotence invariant (V19)**: ctx is updated synchronously on any daemon
  file delete/adopt; the poller echo ≤2 s later is expected and must be a
  no-op (`power_policy_check` is idempotent on unchanged inputs).

### Self-write detection (V7)

`ctx.power_expected`: list of (expiry≈5 s, frozenset of acceptable values) —
the matrix value **plus its full fallback chain** (power-saver → {low-power,
quiet}) — appended before each ApplyProfile call; matched entries removed;
additionally ALL adoption is suppressed within 5 s of any ApplyProfile call.
PLATFORM_PROFILE_CHANGED with a non-expected value outside the window → adopt
as override (map back: low-power|quiet→power-saver, performance→performance,
else balanced), write file + ctx synchronously, notify. Never revert.
GPU-coherence note: on firmware without low-power/quiet, power-saver implies
gpu mode `auto`, not `igpu` (resolve_gpu_mode maps unknown→auto — verified).

### powerd-absent path (V11)

On D-Bus failure: warn once, mark unavailable; subscribe to NameOwnerChanged
for org.hyprstate.Power1 — on appearance, clear the flag and re-apply desired.
`power status --waybar` on failure prints valid JSON (`class: unavailable`)
from the local view and exits 0; plain status prints `powerd: unavailable`.

### Brightness (V16, V18)

- Discovery once at startup: exclude `ddcci*`; prefer
  `amdgpu_bl*|intel_backlight|acpi_video*`; else `sorted()[0]`; log choice.
- Effects on EDGES only: ac→battery save + cap at 50%; battery-low entry →
  25%; battery→ac restore. Manual profile overrides never touch brightness.
- **Takeover guard at ±0.5% of max** (V18 — 2% was blind to 1% user steps):
  current differs from last-set beyond that → user adjusted; skip + clear.
- `set_brightness` is a logged no-op when the logind session proxy is None or
  no backlight exists. On RESUMED, re-read brightness into `brightness_set`
  (re-arm the guard; resume drift must not mis-trip it).
- Mechanism: logind `Session.SetBrightness("backlight", dev, raw)` —
  unprivileged for the session owner.

### Dispatcher integration

- `power_policy_check(ctx, fx)` from: MONITORS_CHANGED branch (after gpu
  breadcrumb/drift, before `continue`) and fall-through gate on
  `(BATTERY_LOW_CHANGED, POWER_OVERRIDE_CHANGED, POWER_AC_SETTLED)`.
- PLATFORM_PROFILE_CHANGED branch: prune expected-set; self-write → skip;
  inside suppression window → skip; else adopt (then evaluate policy directly).
- Startup: initial UPower GetAll → read override file → enqueue one
  POWER_OVERRIDE_CHANGED after the seeded MONITORS_CHANGED.

### CLI

`hyprstate power set <profile>|auto · get · cycle · status [--waybar]`.
cycle: auto→power-saver→balanced→performance→auto. status queries powerd
(GetProfile/GetKnobs) with the V11 failure path. `status_main` gains a power
section.

## Component 3 — chezmoi delivery

- `dot_config/hypr/power.conf` (new, defaults above).
- `dot_config/waybar/config.tmpl`: `custom/power-profile` (exec `hyprstate
  power status --waybar`, exec-if, json, interval 5, signal 9, on-click
  `hyprstate power cycle; pkill -SIGRTMIN+9 waybar`), before battery.
- `dot_config/waybar/style.css`: per-profile + unavailable classes.
- hyprstate repo additionally ships: `hyprstate-powerd.service`,
  `org.hyprstate.Power1.conf`, `org.hyprstate.Power1.service` (bus activation);
  install.sh installs all three + the libexec copy, daemon-reloads, enables,
  and asserts is-active. uninstall mirrors.

## Known accepted limitations

- hypridle timeout switching deferred (restart races lock pipeline + inhibit
  log).
- Charge thresholds / kbd backlight: EC/QMK, future.
- Waybar applied-state lag ≤2 s after click (poller tick); optimistic
  SIGRTMIN+9 refresh covers display.
- powerd code updates require rerunning install.sh (root-owned copy, V3).
