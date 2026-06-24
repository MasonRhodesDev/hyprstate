# hyprstate GUI — live FSM graph + settings editor (spec v2, Rust/Slint)

Status: design of record. Companion to GPU_SPEC.md / POWER_SPEC.md. Supersedes the
discarded TypeScript draft — the project is Rust, so the GUI is Rust + Slint.

## Goal

hyprstate is a state machine (lid/monitor/lock/suspend FSM + DPMS sub-FSM + power
policy + GPU selection) observable today only via logs and scattered state files.
The GUI renders the FSM as a live node graph: current state, recent transitions,
context, and effector firings in real time — answering "why did it pick the iGPU /
clamp the dGPU / suspend" at a glance — plus property panels for the small editable
config surface.

## Mechanism vs configuration (kept separate)

- **Mechanism = the FSM** (`hyprstate-fsm` crate: `State`, `ScreenState`, `EventKind`,
  `world_state`, `desired_state`, `desired_screen_state`). This is code. The GUI
  *renders and observes* it; it is never editable-as-graph.
- **Configuration = data**, and small: `power.conf` (4-row base-state→profile map +
  `battery-low-percent`), `gpu-select` (one enum word), `profiles/*.conf` (monitor
  layouts + `#@` directives). Edited via property panels on the relevant nodes.

## The graph is Moore-style, not an edge table

`world_state(WorldInputs) -> State` derives state from inputs; it is **not** a
hand-maintained `(state,event)→state` table. So the visualization is:
**world-input nodes → state nodes**, plus the **event stream** that triggers
re-evaluation, plus **effector** side-effect nodes. Don't force classic transition
arrows where the code doesn't have them.

## Single source of truth: the shared crate

`hyprstate-fsm` (already extracted, a workspace member of the daemon repo) is the
shared crate. The GUI imports it directly — same `State`/`ScreenState`/`EventKind`
the daemon runs, with `#[derive(Serialize, Deserialize)]` already added. **No
hand-mirrored model, no drift guard.** When the daemon's FSM changes, the GUI's model
changes with it at compile time.

## Live telemetry: daemon → GUI over UDS

The daemon emits a JSON event per transition/tick over a Unix domain socket
(`$XDG_RUNTIME_DIR/hyprstate-telemetry.sock`), matching the `another-one`
`daemon-transport` convention (newline-delimited serde_json frames):

```
{ "ts": <ms>, "kind": "transition", "from": "LID_OPEN", "event": "LidClose",
  "to": "Countdown", "screen": "Active", "ctx": { ...inputs... },
  "effectors": ["arm_grace_timer"] }
```

- A small additive emitter in the daemon (Layer 2 `on_enter` already the single place
  effects fire) writes a frame after each dispatch. Low-risk, separately reviewed.
- Until the emitter lands, the GUI degrades to the read-only sources that already
  exist (`hypr-gpu-primary.json`, `power status --waybar`, `/var/lib/hyprstate/profile`,
  sysfs, `journalctl -o json`), so Phase 1/2 are not blocked on a daemon change.

## Rendering: Slint, native Paths

- **Framework: Slint** (the owner's established framework; layer-shika/ashpd available).
  Graph rendered with Slint's `Path` element — `CubicTo`/`QuadraticTo` are native and
  a `commands` SVG-string property exists, so Bezier wires need no approximation.
  Pan/zoom via `Flickable` + `Transform`; selection/property panels are ordinary Slint.
- egui+egui-snarl was evaluated and rejected: Slint matches the owner's stack and the
  node-graph lift is modest once Beziers are native.

## Testing & headless snapshots

- **Logic is pure and already unit-tested** (`hyprstate-fsm`, 57 tests). Graph layout /
  state-mapping go in plain testable Rust modules.
- **Visual regression is headless** via the standalone `slint-headless` crate
  (surfaceless EGL + FemtoVG → PNG, no compositor) — it renders Slint `Path` items the
  software renderer drops. The GUI uses it as a dev-dependency to snapshot graph states
  and assert via pixel stats / golden images, with no window and no human.
- Manual spot-checks: run windowed under Hyprland; `grim` for a screenshot if wanted.

## Repo & packaging layout

- `hyprstate` repo: daemon binary + `crates/hyprstate-fsm` (workspace). Builds/vendors
  exactly as before — the GUI is **not** a member, so Slint never enters `cargo vendor`
  or the RPM/PKGBUILD build.
- `hyprstate-gui` repo (separate): the Slint app. Depends on `hyprstate-fsm`
  (path/git) and `slint-headless` (dev-dep). Packaged independently.
- `slint-headless` repo (built): the offscreen Slint→PNG snapshot primitive.

## Editing (later phase)

Property panels write through existing surfaces only, atomically (tmp+rename),
validated against the daemon's own whitelists; the daemon's pollers pick up file
changes — the GUI never writes daemon-owned runtime state:
- power node → rewrite `~/.config/hypr/power.conf`; live changes via `hyprstate power set`.
- gpu node → write `~/.config/hypr/gpu-select`.
- monitor-profile nodes → edit `profiles/*.conf`.

## Phasing

1. **Shared model + GUI skeleton** — `hyprstate-fsm` imported; GUI window renders the
   static FSM graph (input/state/effector nodes, Bezier wires); headless snapshot test
   green. ← foundation
2. **Live state** — consume telemetry (or the fallback sources); highlight current
   state, stream events, animate transitions.
3. **Daemon telemetry emitter** — additive UDS JSON emit (separately reviewed).
4. **Editable property panels** — the small config surface above.

## Non-goals

- Editing FSM structure via the GUI; replacing logs as the daemon's record of truth;
  any persistent web/JS toolchain; the GUI entering the daemon's package.

## Autonomous build loop

This project is driven by an automation harness (`hyprstate-gui/automation/`,
TypeScript on Node 24): an ordered task plan, a `verify.ts` gate (build + test +
clippy + headless snapshot across the three repos), and a `drive.ts` orchestrator that
dispatches each task to a headless `pi --print` agent, runs the gate, commits on green,
and halts for review on repeated failure. See that directory's README.
