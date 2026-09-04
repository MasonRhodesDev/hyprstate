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

The package owns a **root-owned binary** at `%{_bindir}/hyprstate`
(`/usr/bin/hyprstate`, root:root, not user-writable); the system unit's
ExecStart runs it directly. The same binary serves the *user* daemon + CLI —
one file, root-owned, so root never executes user-writable code. This replaces
the Python-era dev symlink + `/usr/local/libexec` copy (the symlink targeted a
user-writable file, which is why a separate root-owned copy was needed); under
the RPM/PKGBUILD that concern is moot. powerd code updates ship as a package
update (`dnf`/`pacman`), which restarts the unit via the systemd scriptlets.

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
unit as started). A malformed policy should fail loudly post-install
(`systemctl is-active hyprstate-powerd`). Additionally ship
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
ExecStart=/usr/bin/hyprstate powerd, StateDirectory=hyprstate,
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
#@ lid = present
```

Missing file/keys → defaults above; values validated ∈ profiles. `battery*`
keys inert on desktops — deliberate, not templated (V20c).

**`lid = present|absent`** (default `present`). `absent` declares a lidless
machine: the `handle-lid-switch` block inhibitor is not taken, the lid watcher
is not spawned, lid events warn-and-ignore, and the FSM's lid route is
therefore dead. There is no auto-probe — a mistaken "absent" is harmless
(no lid to mishandle) but a mistaken "present" is too (today's default), while
a *false absent from a probe* on a real laptop would let logind suspend it
unlocked on lid close, so absence must be declared, never guessed.

### The idle/power ladder — decided model (2026-09-04)

Reviewed and decided by Mason on the lavish decision graph
(`~/repos/.lavish/idle-power-model.html`); this section is the recorded form
and supersedes any earlier precedence text it contradicts.

**Core rule: keep-awake has exactly one power.** A keep-awake claim — an app
idle-inhibitor (Wayland surface, ScreenSaver D-Bus, logind idle-block) or the
user's deliberate toggle, *indistinguishable by design* — prevents entry into
the warn→lock ladder while the user might still be present. That is all it
does. **Once the session is LOCKED, no claim is consulted again**: the screen
blanks 30 s later unconditionally (toggle included), and 900 s of true input
idle suspends unconditionally. Claims govern the unlocked machine only.

The ladder (any input returns to AWAKE and cancels a warn or grace in
flight):

```
input < 180s ─────────────────────────────▶ AWAKE
no input 180s ── claim held? ── yes ──▶ HELD_AWAKE (lit, unlocked)
                     │                        │ claim released: TRUE idle
                     no                       ▼ clock — brief warn, prompt lock
                     ▼
                 WARN (blur ramp) ──▶ LOCK ──▶ +30s: BLANK (always)
                                        │
                                        └──▶ 900s total idle: GRACE (30 s,
                                             live locker proven) ──▶ SUSPEND
```

The seven recorded decisions:

1. **A locked screen always blanks**; keep-awake only prevents lock and
   suspend. The blanker's user-toggle re-admission is removed.
2. **Claim release acts on true input-idle.** A video ending 50 minutes after
   the user left gets one brief blur warning, then a prompt lock; past 900 s
   the suspend follows promptly. Release is not activity.
3. **Lock ends every claim's authority.** The 900 s suspend trigger ignores
   inhibitors and gates on the compositor lock instead — the same shape as
   the blanker.
4. **Docked follows the same ladder.** A genuinely idle docked laptop
   suspends at 900 s like the desktop: a standing suspend request outranks
   `Docked` in `world_state`. Lid-close while docked still triggers nothing —
   Docked only neutralizes the lid as a suspend *trigger*. Lid-close on an
   undocked laptop with a claim held (a call) parks in `Deferred` until the
   claim releases: claims govern the unlocked machine, and only it.
5. **Local input only.** Remote/SSH activity is not presence; remote users
   claim keep-awake explicitly (toggle or `systemd-inhibit`).
6. **Battery-low overrides keep-awake.** On battery below the low threshold
   the daemon self-requests suspend; that request bypasses the claim gate
   (`Countdown` even under an inhibitor), and the suspend machinery locks
   first as always.
7. **The awake state must explain itself.** `hyprstate status` and the
   telemetry stream name the current ladder node and every keep-awake holder
   (feeds the dials lit `idle_graph()`); sensing is unified so the daemon
   sees the same claim set hypridle honors; resume re-arms the ladder
   (hypr-DE#29).

#### Decision audit matrix

Every decision maps to executable checks; "will this be auditable against a
testable playbook" is answered by running them:

| # | Decision | Executable checks |
|---|----------|-------------------|
| 1 | Locked always blanks | hypr-DE `tests/lock-policy.sh` (locked + toggle-held must blank); registry `ladder-locked-screen-always-blanks` |
| 2 | Claim release acts on true input-idle | hypr-DE `no-keep-awake.sh` gate + `condition_retry=10` on the 180 s listener (release → warn + lock within ~10 s); lock-policy.sh per-source gate tests; registry `ladder-warn-gated-on-claims`. Residual gap: the D-Bus-ledger release path (hypridle internal) is still unpinned |
| 3 | Lock ends claim authority | fsm test `lock_ends_a_claims_authority`; lock-policy.sh suspend-block structural check; registry `ladder-suspend-listener-ignores-claims`, `ladder-claims-govern-only-the-unlocked-machine` |
| 4 | Docked follows the ladder | fsm tests `a_request_outranks_docked`, `a_docked_laptop_with_a_request_suspends` (documented reversal of review #6) |
| 5 | Local input only | Doc-only by design — no code path senses remote activity, so there is nothing to test |
| 6 | Battery-low overrides claims | fsm test `battery_low_overrides_the_claim`; dispatcher `battery_low_tests` (request + withdraw table, review F1/F2); registry `ladder-battery-request-withdraws-on-recovery` |
| 7 | Holder-naming observability | **PENDING** — lands with the Q7 sensing/telemetry PR, with its own checks |

Plus the standing suspend-safety assertions (single `do_suspend` site,
Resumed clears the request, no direct suspend in hypridle.conf, no effector
self-call) in the desktop-commons registry.

Known audit gaps, on record: decision 2's D-Bus-ledger release path is
hypridle-internal and unpinned (the stateless-gate sources are covered); the
daemon's dispatcher layer has no test harness (the battery-low F1/F2 bugs
were caught by adversarial review, not tests — the pure
`battery_low_action` extraction is the mitigation); there is no automated
end-to-end seat test of the ladder timing.

Rationale on record: the 2026-09-03 incident — hours unlocked-and-lit because
an unnameable app claim blocked the 180 s lock — was wrong twice under this
model: the claim outlived real absence with unlimited authority, and nothing
could name the claimant. Decisions 2+3 bound every claim's authority at the
lock; decision 7 makes the one remaining held state diagnosable.

### Idle-suspend request

`hyprstate suspend request|cancel` writes/removes
`$XDG_RUNTIME_DIR/hyprstate-suspend-request` (runtime dir so a reboot clears
it). A standing request is a `WorldInputs` field that drives `world_state` to
`Countdown` ahead of the lid chain *and ahead of Docked* (decision 4); only a
keep-awake claim on a still-unlocked machine parks it in `Deferred` — and a
battery-low request bypasses even that (decision 6). It is a *request* into
the existing machinery: grace window, `LockedHint`+`hyprctl locked` proof,
cancellation, and the single `do_suspend` call all belong to the lid path
already. hypridle drives it (900 s idle → request, on-resume → cancel); the
daemon clears it on `Resumed` and at startup so a wake or a restart never
re-suspends.

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
- hyprstate ships (from `dist/`, installed by the RPM spec / Arch PKGBUILD):
  `hyprstate-powerd.service`, `org.hyprstate.Power1.conf`,
  `org.hyprstate.Power1.service` (bus activation), plus systemd presets. The
  package scriptlets daemon-reload, enable, and restart the units; packaging
  asserts the powerd unit's bus policy is well-formed (a malformed policy would
  let the Type=dbus unit report started without owning the name).

## Known accepted limitations

- **Battery-low with a broken locker retries forever.** Decision 6's suspend
  cannot proceed without a proven live locker (fail-closed is the invariant),
  so a machine whose locker is broken re-arms the 30 s grace and asks
  `Session.Lock()` indefinitely while the battery drains. Failing safe beats
  suspending unlocked; a bounded retry + notification is future work.

- hypridle timeout switching deferred (restart races lock pipeline + inhibit
  log).
- Charge thresholds / kbd backlight: EC/QMK, future.
- Waybar applied-state lag ≤2 s after click (poller tick); optimistic
  SIGRTMIN+9 refresh covers display.
- powerd code updates ship as a package update (`dnf`/`pacman`); the systemd
  scriptlets restart the root-owned unit (V3).
