#!/usr/bin/env python3
"""
hyprstate: lid / monitor / profile / lock / suspend / wake state machine for Hyprland.

Subcommands:
    daemon            run the FSM (via systemd --user)
    sleep-hook ARG    invoked by /usr/lib/systemd/system-sleep/ as root
    install           idempotent install (delegates to install.sh)
    uninstall         reverse install
    status            short systemctl + journalctl summary
    profile           list / show / switch monitor profiles
    gpu               select / check / status for GPU-primary selection

Daemon owns:
    - eDP-2 enable/disable
    - 30s grace window between lid close and suspend
    - Idle-inhibitor-aware deferral with media auto-pause
    - Lock-before-suspend (Session.Lock + 2s wait for LockedHint)
    - DPMS-off sub-FSM when locked + inhibitor with active screens
    - logind handle-lid-switch inhibitor lock (held for process lifetime)
    - Monitor-profile selection by detected-output signature
    - GPU-selection drift detection (notify-only; AQ_DRM_DEVICES is login-time)

Architecture (daemon):
    Layer 1 — Effectors:        narrow, idempotent world mutations.
    Layer 2 — on_enter_<STATE>: composes effectors; the only place side-effects fire.
    Layer 3 — desired_state:    pure (state, ctx) -> state map; no I/O.

Two FSMs run in one dispatcher: a main FSM (lid/monitor/suspend) and a sub-FSM
(screen DPMS). They share ctx and the event queue.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import logging
import os
import re
import shutil
import subprocess
import sys
import time
from collections import deque
from dataclasses import dataclass
from enum import Enum
from pathlib import Path

LOG = logging.getLogger("hyprstate")

# =========================================================================
# Constants
# =========================================================================

EDP_MONITOR = "eDP-2"
MONITORS_CONF = Path.home() / ".config/hypr/configs/monitors.conf"
HYPRIDLE_LOG = Path.home() / ".config/hypr/logs/hypridle.log"
PROFILES_DIR = Path.home() / ".config/hypr/profiles"
ACTIVE_PROFILE_LINK = PROFILES_DIR / ".active.conf"

# GPU selection. The override and breadcrumb files are runtime user state
# (deliberately not chezmoi-managed); the state file is the contract between
# `gpu select` (run pre-compositor by uwsm's env-hyprland) and the daemon's
# drift detection. See GPU_SPEC.md.
GPU_OVERRIDE_FILE = Path.home() / ".config/hypr/gpu-select"
GPU_BREADCRUMB_FILE = Path.home() / ".config/hypr/gpu-profile"
PLATFORM_PROFILE_PATH = Path("/sys/firmware/acpi/platform_profile")
DRI_BY_PATH = Path("/dev/dri/by-path")
GPU_NOTIFY_MIN_SECONDS = 60
GPU_SETTLE_SECONDS = 0.5

GRACE_SECONDS = 30
DPMS_DELAY_SECONDS = 30
LOCK_WAIT_SECONDS = 2.0
INHIBIT_POLL_SECONDS = 2
RECONCILE_SECONDS = 5
PROFILE_DEBOUNCE_SECONDS = 0.5  # coalesce monitor add/remove bursts

INHIBIT_BASELINE_WHO = frozenset({
    "ModemManager",
    "NetworkManager",
    "UPower",
    "hypridle",
    "logind-idle-control",
    "hyprstate",
    "hypr-power",  # transitional; predecessor name
    "hypr-fsm",    # transitional; earlier predecessor
})

LOGIND_BUS = "org.freedesktop.login1"
LOGIND_PATH = "/org/freedesktop/login1"
LOGIND_IFACE = "org.freedesktop.login1.Manager"
SESSION_IFACE = "org.freedesktop.login1.Session"

UPOWER_BUS = "org.freedesktop.UPower"
UPOWER_PATH = "/org/freedesktop/UPower"
UPOWER_IFACE = "org.freedesktop.UPower"

# /sys/.../power/wakeup paths covered by the sleep hook. Keeping this list in
# one place lets the daemon's startup diagnostic and the hook walk the same
# devices.
WAKE_USB_VENDORS = {
    ("3297", "1977"): "ZSA Voyager (keyboard)",
    ("046d", "c539"): "Logitech Lightspeed (mouse receiver)",
}
WAKE_USB_CONTROLLER = "/sys/bus/pci/devices/0000:0e:00.3/power/wakeup"

# =========================================================================
# Daemon: states & events
# =========================================================================


class State(Enum):
    LID_OPEN = "LID_OPEN"
    DOCKED = "DOCKED"
    DEFERRED = "DEFERRED"
    COUNTDOWN = "COUNTDOWN"
    SUSPENDING = "SUSPENDING"


class ScreenState(Enum):
    ACTIVE = "SCREEN_ACTIVE"
    DIM_PENDING = "SCREEN_DIM_PENDING"
    DIMMED = "SCREEN_DIMMED"


class EventKind(Enum):
    LID_CLOSE = "LidClose"
    LID_OPEN = "LidOpen"
    MONITOR_ADDED = "MonitorAdded"
    MONITOR_REMOVED = "MonitorRemoved"
    INHIBITOR_ON = "InhibitorOn"
    INHIBITOR_OFF = "InhibitorOff"
    LOCK_ENGAGED = "LockEngaged"
    LOCK_RELEASED = "LockReleased"
    AC_PLUGGED = "AcPlugged"
    AC_UNPLUGGED = "AcUnplugged"
    TIMER_EXPIRED = "TimerExpired"
    SCREEN_TIMER_EXPIRED = "ScreenTimerExpired"
    RESUMED = "Resumed"
    RECONCILE = "Reconcile"
    MONITORS_CHANGED = "MonitorsChanged"  # debounced; fires on add/remove bursts
    PLATFORM_PROFILE_CHANGED = "PlatformProfileChanged"
    GPU_OVERRIDE_CHANGED = "GpuOverrideChanged"


@dataclass
class Event:
    kind: EventKind
    payload: object = None


@dataclass
class Context:
    lid_closed: bool = False
    ext_mon_count: int = 0
    logind_inhibitor: bool = False
    wayland_inhibitor: bool = False
    locked: bool = False
    on_ac: bool = True  # observed but not currently consumed by transitions
    timer_task: asyncio.Task | None = None
    screen_timer_task: asyncio.Task | None = None
    state: State | None = None
    screen_state: ScreenState = ScreenState.ACTIVE

    # Profile sub-FSM: current_profile is the name of the .conf in PROFILES_DIR
    # currently pointed at by .active.conf (or None if unmanaged). edp_policy is
    # set from the active profile's `#@ edp =` directive and mediates set_edp().
    current_profile: str | None = None
    edp_policy: str = "auto"  # "auto" | "enable" | "disable"
    profile_debounce_task: asyncio.Task | None = None

    # GPU drift detection. gpu_actual is the AQ_DRM_DEVICES list read from
    # Hyprland's own environ (ground truth): pending until first resolved;
    # None = unmanaged session (drift checks disabled); [] = compositor
    # defaults but advice still wanted (post transient-bail select).
    # gpu_last_notified debounces per desired-list; reset on sync so a
    # re-dock after convergence notifies again.
    gpu_actual: list[str] | None = None
    gpu_actual_pending: bool = True
    gpu_last_notified: str | None = None
    gpu_last_notify_at: float = 0.0

    @property
    def inhibitor(self) -> bool:
        return self.logind_inhibitor or self.wayland_inhibitor


# =========================================================================
# Monitor profiles
# =========================================================================
#
# A profile is a single .conf in PROFILES_DIR with `#@ key = value` directive
# comments at the top. Hyprland sources the active profile via a symlink
# (.active.conf -> the chosen file) which the daemon repoints. Selection is
# pure: given the set of detected monitor descriptions, pick the profile with
# the most specific match (or highest explicit `#@ priority`).


@dataclass(frozen=True)
class Profile:
    name: str
    path: Path
    matches: tuple[str, ...]
    edp: str  # "auto" | "enable" | "disable"
    hooks: tuple[str, ...]
    priority: int  # explicit `#@ priority`; defaults to len(matches)
    gpu: str = "auto"  # "auto" | "igpu" | "dgpu" — render-GPU preference


_DIRECTIVE_RE = re.compile(r"^#@\s*([a-z]+)\s*=\s*(.+?)\s*$")


def load_profiles(profiles_dir: Path = PROFILES_DIR) -> list[Profile]:
    """Read every *.conf in PROFILES_DIR (excluding the `.active.conf` symlink
    and any leading-dot file). Malformed profiles are logged and skipped."""
    profiles: list[Profile] = []
    if not profiles_dir.is_dir():
        return profiles
    for path in sorted(profiles_dir.glob("*.conf")):
        if path.name.startswith("."):
            continue
        try:
            profiles.append(_parse_profile(path))
        except Exception as e:
            LOG.warning("skipping malformed profile %s: %s", path, e)
    return profiles


def _parse_profile(path: Path) -> Profile:
    matches: list[str] = []
    hooks: list[str] = []
    edp = "auto"
    gpu = "auto"
    priority: int | None = None
    with path.open() as fh:
        for raw in fh:
            line = raw.rstrip("\n")
            if not line.startswith("#@"):
                # Stop scanning once the body begins. Profile directives must
                # all sit in the leading comment block — anything below is
                # passed through to Hyprland as-is.
                if line.lstrip().startswith("#") or not line.strip():
                    continue
                break
            m = _DIRECTIVE_RE.match(line)
            if not m:
                LOG.warning("%s: ignoring malformed directive: %r", path.name, line)
                continue
            key, val = m.group(1), m.group(2)
            if key == "match":
                matches.append(val)
            elif key == "hook":
                hooks.append(val)
            elif key == "edp":
                if val not in ("auto", "enable", "disable"):
                    raise ValueError(f"edp must be auto|enable|disable, got {val!r}")
                edp = val
            elif key == "gpu":
                if val not in ("auto", "igpu", "dgpu"):
                    raise ValueError(f"gpu must be auto|igpu|dgpu, got {val!r}")
                gpu = val
            elif key == "priority":
                priority = int(val)
            else:
                LOG.warning("%s: unknown directive %r", path.name, key)
    if not matches:
        raise ValueError("profile has no `#@ match = ...` directives")
    return Profile(
        name=path.stem,
        path=path,
        matches=tuple(matches),
        edp=edp,
        hooks=tuple(hooks),
        priority=priority if priority is not None else len(matches),
        gpu=gpu,
    )


def select_profile(
    signature: frozenset[str], profiles: list[Profile]
) -> Profile | None:
    """Pure: pick the profile whose match set is a subset of `signature`,
    breaking ties by `priority` (descending). `signature` is the set of
    monitor descriptions reported by `hyprctl monitors -j` (full strings)."""
    candidates = [
        p for p in profiles
        if all(_match_in_signature(m, signature) for m in p.matches)
    ]
    if not candidates:
        return None
    return max(candidates, key=lambda p: (p.priority, len(p.matches), p.name))


def _match_in_signature(match: str, signature: frozenset[str]) -> bool:
    """A `#@ match = ...` directive matches if any detected monitor description
    starts with the directive's value. The `desc:` prefix (Hyprland syntax) is
    stripped for comparison so users can paste rules from monitors.conf
    verbatim."""
    needle = match.removeprefix("desc:").strip()
    return any(desc.startswith(needle) for desc in signature)


def monitor_signature() -> frozenset[str]:
    """Snapshot of currently-connected monitor descriptions from hyprctl."""
    try:
        out = run(["hyprctl", "-j", "monitors"]).stdout
        return frozenset(m.get("description", "") for m in json.loads(out))
    except Exception as e:
        LOG.warning("monitor_signature failed: %s", e)
        return frozenset()


# =========================================================================
# GPU-primary selection (see GPU_SPEC.md)
# =========================================================================
#
# `gpu select` runs pre-compositor (uwsm env-hyprland): no Hyprland, no
# daemon, no D-Bus — sysfs only, stdout is consumed raw by the shell caller,
# so this section must never write anything but the device list to stdout.
# The daemon reuses the same pure pipeline for drift detection at runtime.


@dataclass(frozen=True)
class GpuCard:
    path: str  # stable /dev/dri/by-path entry
    card: str  # resolved cardN (NOT stable across boots; sysfs lookup only)
    boot_vga: int
    vram: int
    external: int  # connected non-eDP connectors
    edp: int       # connected eDP connectors


@dataclass(frozen=True)
class GpuSnapshot:
    cards: tuple[GpuCard, ...]
    non_pci_display: bool  # a non-candidate DRM device has a connected output
    lid_closed: bool


def _read_int(path: Path, default: int = 0) -> int:
    try:
        return int(path.read_text().strip(), 0)
    except (OSError, ValueError):
        return default


def _read_first_word(path: Path) -> str | None:
    try:
        words = path.read_text().split()
        return words[0] if words else None
    except OSError:
        return None


def _card_connectors(card: str) -> tuple[int, int]:
    """(external, edp) connected-connector counts for cardN."""
    external = edp = 0
    for status in Path("/sys/class/drm").glob(f"{card}-*/status"):
        conn = status.parent.name  # e.g. card1-DP-1
        if "-Writeback-" in conn:
            continue
        try:
            if status.read_text().strip() != "connected":
                continue
        except OSError:
            continue
        if "eDP" in conn:
            edp += 1
        else:
            external += 1
    return external, edp


def _lid_closed_sysfs() -> bool:
    """Lid state without logind (D-Bus is unavailable at select time)."""
    try:
        for state in Path("/proc/acpi/button/lid").glob("*/state"):
            return "closed" in state.read_text()
    except OSError:
        pass
    return False


def gpu_snapshot() -> GpuSnapshot:
    """Enumerate GPU candidates from /dev/dri/by-path. A candidate must be a
    PCI display-class device (class 0x03*) with no usb segment in its by-path
    name — this excludes DisplayLink/evdi and platform devices, which we never
    list in AQ_DRM_DEVICES (untested scanout path). If such a non-candidate
    has a connected output, the snapshot flags it so selection can bail to
    today's open-all-GPUs behavior instead of killing that output."""
    cards: list[GpuCard] = []
    seen: set[str] = set()
    non_pci_display = False
    try:
        entries = sorted(DRI_BY_PATH.glob("*-card"))
    except OSError:
        entries = []
    for link in entries:
        card = os.path.realpath(link).rsplit("/", 1)[-1]
        if not card.startswith("card") or card in seen:
            continue
        seen.add(card)
        external, edp = _card_connectors(card)
        dev = Path("/sys/class/drm") / card / "device"
        pci_class = ""
        try:
            pci_class = (dev / "class").read_text().strip()
        except OSError:
            pass
        name = link.name
        is_candidate = (
            name.startswith("pci-")
            and "-usb-" not in name
            and "-usbv2-" not in name
            and pci_class.startswith("0x03")
        )
        if not is_candidate:
            if external or edp:
                non_pci_display = True
            continue
        cards.append(GpuCard(
            path=str(link),
            card=card,
            boot_vga=_read_int(dev / "boot_vga"),
            vram=_read_int(dev / "mem_info_vram_total"),
            external=external,
            edp=edp,
        ))
    return GpuSnapshot(tuple(cards), non_pci_display, _lid_closed_sysfs())


def resolve_gpu_mode(profile_overlay: str | None = None) -> tuple[str, str]:
    """-> (mode, source). Precedence: override file > profile preference >
    platform_profile > auto.

    profile_overlay: the daemon passes the freshly-selected profile's `#@ gpu`
    value ("auto" when no profile matches — never the stale ctx value). When
    None (CLI / pre-compositor, where profiles can't be matched), the
    breadcrumb file written by the daemon stands in for it, keeping desired
    and next-login select computing from identical inputs."""
    word = _read_first_word(GPU_OVERRIDE_FILE)
    if word is not None:
        if word in ("igpu", "dgpu", "off"):
            return word, "override"
        if word != "auto":
            LOG.warning("%s: ignoring unknown mode %r", GPU_OVERRIDE_FILE, word)
    overlay = (profile_overlay if profile_overlay is not None
               else _read_first_word(GPU_BREADCRUMB_FILE))
    if overlay in ("igpu", "dgpu"):
        return overlay, "profile"
    word = _read_first_word(PLATFORM_PROFILE_PATH)
    if word in ("low-power", "quiet"):
        return "igpu", "platform"
    if word == "performance":
        return "dgpu", "platform"
    # balanced / balanced-performance / cool / custom / missing / unknown:
    # deliberately auto (exhaustive against the platform_profile ABI).
    return "auto", "default"


def _integrated_card(cards: tuple[GpuCard, ...]) -> GpuCard | None:
    """Integrated = boot_vga AND smallest-VRAM agreeing on the same card.
    Disagreement (e.g. a muxed laptop reporting boot_vga on the discrete) or
    a VRAM tie without boot_vga -> None -> unmanaged. Cards lacking
    mem_info_vram_total (Intel, nouveau) read as 0, which agrees trivially."""
    min_vram = min(c.vram for c in cards)
    by_vram = [c for c in cards if c.vram == min_vram]
    by_vga = [c for c in cards if c.boot_vga == 1]
    if len(by_vga) == 1:
        return by_vga[0] if by_vga[0] in by_vram else None
    if not by_vga and len(by_vram) == 1:
        return by_vram[0]
    return None


def gpu_desired(snap: GpuSnapshot, mode: str, source: str) -> tuple[list[str] | None, str]:
    """Pure: (device list primary-first, reason) or (None, reason) = unmanaged
    (caller prints nothing; Hyprland falls back to its own defaults)."""
    if mode == "off":
        return None, "override-off"
    if len(snap.cards) < 2:
        return None, "no-multi-gpu"
    if snap.non_pci_display:
        return None, "non-pci-display-present"
    integrated = _integrated_card(snap.cards)
    if integrated is None:
        return None, "ambiguous-integrated"
    if snap.lid_closed and not any(c.external for c in snap.cards):
        # Docked cold boot: DP links can still be down at early-login sysfs
        # read. Omitting the dock's GPU here would leave a lid-closed session
        # with no usable output (and the lid FSM would suspend-loop). The
        # caller does one settle retry; persistent -> unmanaged.
        return None, "bailed-transient"

    discretes = sorted(
        (c for c in snap.cards if c is not integrated),
        key=lambda c: (-c.external, -c.vram, c.path),
    )
    best = discretes[0]

    if mode == "auto":
        if best.external or best.edp:
            primary, reason = best, "dgpu-has-display"
        else:
            primary, reason = integrated, "dgpu-idle-omitted"
    elif mode == "igpu":
        primary, reason = integrated, f"{source}-igpu"
    else:  # dgpu: forced on (and kept awake) even with no display
        primary, reason = best, f"{source}-dgpu"

    devices = [primary]
    if integrated is not primary:
        devices.append(integrated)  # integrated always listed (eDP/hotplug)
    for c in discretes:
        if c is primary:
            continue
        if c.external or c.edp:
            devices.append(c)  # display-less discretes omitted -> runtime PM

    # Usable-output invariant: never emit a list under which nothing can
    # light up — a connected external on a listed card, or eDP with lid open.
    usable = any(c.external for c in devices) or (
        not snap.lid_closed and any(c.edp for c in devices)
    )
    if not usable:
        return None, "bailed-transient"
    return [c.path for c in devices], reason


def _write_gpu_state(mode: str, reason: str, devices: list[str] | None,
                     snap: GpuSnapshot) -> None:
    """Best-effort atomic intent record for the daemon/status. Never raises,
    never touches stdout."""
    try:
        integrated = _integrated_card(snap.cards) if len(snap.cards) >= 2 else None
        payload = {
            "version": 1,
            "mode": mode,
            "reason": reason,
            "primary": devices[0] if devices else None,
            "devices": devices or [],
            "omitted": [c.path for c in snap.cards
                        if not devices or c.path not in devices],
            "snapshot": {
                c.path.rsplit("/", 1)[-1].removesuffix("-card"): {
                    "type": "integrated" if c is integrated else "discrete",
                    "boot_vga": c.boot_vga,
                    "vram": c.vram,
                    "external": c.external,
                    "edp": c.edp,
                }
                for c in snap.cards
            },
        }
        state = gpu_state_path()
        tmp = state.with_suffix(".tmp")
        tmp.write_text(json.dumps(payload, indent=1))
        tmp.replace(state)
    except Exception as e:
        LOG.warning("gpu state write failed: %s", e)


def gpu_state_path() -> Path:
    return Path(os.environ.get("XDG_RUNTIME_DIR", "/tmp")) / "hypr-gpu-primary.json"


def _hyprland_pid() -> int | None:
    """PID from the instance lock file. pgrep is forbidden here: nested
    Hyprland instances are a known use pattern and comm matching can't
    distinguish them; the lock file is scoped by instance signature."""
    sig = os.environ.get("HYPRLAND_INSTANCE_SIGNATURE")
    runtime = os.environ.get("XDG_RUNTIME_DIR")
    if not sig or not runtime:
        return None
    lock = Path(runtime) / "hypr" / sig / "hyprland.lock"
    try:
        pid = int(lock.read_text().splitlines()[0].strip())
    except (OSError, ValueError, IndexError):
        return None
    try:
        if Path(f"/proc/{pid}/comm").read_text().strip() != "Hyprland":
            return None
    except OSError:
        return None
    return pid


def _hyprland_aq_devices() -> list[str] | None:
    """AQ_DRM_DEVICES from Hyprland's own environ — ground truth for what the
    session actually uses (the state file is only intent). None = no confirmed
    Hyprland yet; [] = running with the var unset (compositor defaults)."""
    pid = _hyprland_pid()
    if pid is None:
        return None
    try:
        environ = Path(f"/proc/{pid}/environ").read_bytes()
    except OSError:
        return None
    for chunk in environ.split(b"\0"):
        if chunk.startswith(b"AQ_DRM_DEVICES="):
            val = chunk.split(b"=", 1)[1].decode(errors="replace")
            return [p for p in val.split(":") if p]
    return []


# =========================================================================
# Daemon: effectors (Layer 1)
# =========================================================================


class Effectors:
    def __init__(self, bus, manager_iface, session_iface, queue: asyncio.Queue):
        self.bus = bus
        self.manager = manager_iface
        self.session = session_iface
        self.queue = queue
        self._lid_inhibit_fd: int | None = None

    async def take_lid_inhibitor(self) -> None:
        """Hold a block-mode handle-lid-switch inhibitor for our process lifetime."""
        fd = await self.manager.call_inhibit(
            "handle-lid-switch",
            "hyprstate",
            "30s grace window with monitor/inhibitor cancellation",
            "block",
        )
        self._lid_inhibit_fd = fd
        LOG.info("acquired handle-lid-switch inhibitor (fd=%d)", fd)

    def set_edp(self, on: bool, ctx: Context | None = None) -> None:
        # The active profile's `#@ edp` directive overrides the lid-driven
        # default: docked profiles ("disable") keep eDP off even when the lid
        # opens; "enable" forces it on; "auto" defers to the caller.
        if ctx is not None:
            if ctx.edp_policy == "disable":
                on = False
            elif ctx.edp_policy == "enable":
                on = True
        current_disabled = _edp_is_disabled()
        if current_disabled is None:
            return  # eDP monitor not present; nothing to do
        if on:
            if current_disabled is False:
                return
            LOG.info("re-enabling %s via hyprctl reload", EDP_MONITOR)
            run(["hyprctl", "reload"], check=False)
        else:
            if current_disabled is True:
                return
            LOG.info("disabling %s", EDP_MONITOR)
            run(["hyprctl", "keyword", "monitor", f"{EDP_MONITOR},disable"], check=False)

    def apply_profile(self, profile: Profile, ctx: Context) -> None:
        """Repoint .active.conf at `profile.path`, run `hyprctl reload`, fire
        post-apply hooks, then update ctx.current_profile / edp_policy.
        Idempotent: a no-op when already pointing at the same profile."""
        target = profile.path
        link = ACTIVE_PROFILE_LINK
        try:
            if link.is_symlink() and link.resolve() == target.resolve():
                if ctx.current_profile == profile.name:
                    return  # already applied
            link.parent.mkdir(parents=True, exist_ok=True)
            tmp = link.with_suffix(link.suffix + ".tmp")
            if tmp.exists() or tmp.is_symlink():
                tmp.unlink()
            tmp.symlink_to(target)
            tmp.replace(link)  # atomic
        except OSError as e:
            LOG.error("apply_profile %s: symlink failed: %s", profile.name, e)
            return

        LOG.info("PROFILE: %s -> %s (edp=%s, hooks=%d)",
                 ctx.current_profile, profile.name, profile.edp, len(profile.hooks))
        ctx.current_profile = profile.name
        ctx.edp_policy = profile.edp

        run(["hyprctl", "reload"], check=False)
        for cmd in profile.hooks:
            try:
                # Profiles run in user context; shell to allow ~ expansion etc.
                # Hooks are user-authored so this is no broader than what they
                # can already write into the .conf body.
                subprocess.Popen(["bash", "-lc", cmd])
            except Exception as e:
                LOG.warning("hook %r failed to launch: %s", cmd, e)

    def schedule_profile_reconcile(self, ctx: Context) -> None:
        """Debounce monitor add/remove bursts. Multiple events within
        PROFILE_DEBOUNCE_SECONDS coalesce into one MONITORS_CHANGED."""
        if ctx.profile_debounce_task and not ctx.profile_debounce_task.done():
            ctx.profile_debounce_task.cancel()
        ctx.profile_debounce_task = asyncio.create_task(
            self._profile_debounce_coro()
        )

    async def _profile_debounce_coro(self) -> None:
        try:
            await asyncio.sleep(PROFILE_DEBOUNCE_SECONDS)
            await self.queue.put(Event(EventKind.MONITORS_CHANGED))
        except asyncio.CancelledError:
            pass

    def cancel_timer(self, ctx: Context) -> None:
        if ctx.timer_task and not ctx.timer_task.done():
            ctx.timer_task.cancel()
        ctx.timer_task = None

    def start_grace_timer(self, ctx: Context) -> None:
        self.cancel_timer(ctx)
        ctx.timer_task = asyncio.create_task(
            self._timer_coro(GRACE_SECONDS, EventKind.TIMER_EXPIRED)
        )

    def cancel_screen_timer(self, ctx: Context) -> None:
        if ctx.screen_timer_task and not ctx.screen_timer_task.done():
            ctx.screen_timer_task.cancel()
        ctx.screen_timer_task = None

    def start_screen_timer(self, ctx: Context) -> None:
        self.cancel_screen_timer(ctx)
        ctx.screen_timer_task = asyncio.create_task(
            self._timer_coro(DPMS_DELAY_SECONDS, EventKind.SCREEN_TIMER_EXPIRED)
        )

    async def _timer_coro(self, seconds: float, kind: EventKind) -> None:
        try:
            await asyncio.sleep(seconds)
            await self.queue.put(Event(kind))
        except asyncio.CancelledError:
            pass

    def write_gpu_breadcrumb(self, value: str | None) -> None:
        """Record the active profile's `#@ gpu` preference for the next
        login's `gpu select` (profiles can't be matched pre-compositor).
        Cleared on no-match or "auto" so the file only exists when it means
        something — this is what keeps desired and next-login select
        computing from identical inputs (no relog loop)."""
        try:
            if value in (None, "auto"):
                GPU_BREADCRUMB_FILE.unlink(missing_ok=True)
            else:
                GPU_BREADCRUMB_FILE.parent.mkdir(parents=True, exist_ok=True)
                GPU_BREADCRUMB_FILE.write_text(value + "\n")
        except OSError as e:
            LOG.warning("gpu breadcrumb write failed: %s", e)

    def notify_gpu_drift(self, desired: list[str], reason: str, trigger: str,
                         on_ac: bool) -> None:
        """Narrow effect: tell the user a relog would change the render GPU.
        Never acts on the session itself — AQ_DRM_DEVICES is login-time only."""
        primary = desired[0].rsplit("/", 1)[-1].removesuffix("-card")
        body = f"{trigger}: a relog would switch rendering to {primary} ({reason})"
        if not on_ac:
            body += " — on battery"
        LOG.info("GPU drift: desired=%s reason=%s trigger=%s",
                 ":".join(desired), reason, trigger)
        run(["notify-send", "-a", "hyprstate",
             "-h", "string:x-canonical-private-synchronous:hyprstate-gpu",
             "GPU selection drift", body], check=False)

    def pause_media(self) -> None:
        run(["playerctl", "--all-players", "pause"], check=False)

    def dpms(self, on: bool) -> None:
        run(["hyprctl", "dispatch", "dpms", "on" if on else "off"], check=False)

    async def request_lock(self) -> None:
        """Trigger logind's Lock signal. hypridle's lock_cmd then runs hyprlock."""
        if self.session is None:
            LOG.warning("no session proxy — cannot request lock")
            return
        LOG.info("requesting Session.Lock()")
        try:
            await self.session.call_lock()
        except Exception as e:
            LOG.warning("Session.Lock() failed: %s", e)

    async def wait_for_lock(self, ctx: Context, timeout: float = LOCK_WAIT_SECONDS) -> bool:
        """Poll ctx.locked until True or timeout. Yields so the lock watcher can
        update ctx via its PropertiesChanged callback."""
        loop = asyncio.get_event_loop()
        deadline = loop.time() + timeout
        while not ctx.locked:
            if loop.time() >= deadline:
                return False
            await asyncio.sleep(0.05)
        return True

    async def do_suspend(self) -> None:
        LOG.info("calling logind Suspend()")
        await self.manager.call_suspend(False)


# =========================================================================
# Daemon: on_enter handlers (Layer 2)
# =========================================================================


async def on_enter(state: State, ctx: Context, fx: Effectors) -> None:
    handler = {
        State.LID_OPEN: _on_enter_lid_open,
        State.DOCKED: _on_enter_docked,
        State.DEFERRED: _on_enter_deferred,
        State.COUNTDOWN: _on_enter_countdown,
        State.SUSPENDING: _on_enter_suspending,
    }[state]
    await handler(ctx, fx) if asyncio.iscoroutinefunction(handler) else handler(ctx, fx)
    if state is State.SUSPENDING:
        await _suspending_tail(ctx, fx)


def _on_enter_lid_open(ctx: Context, fx: Effectors) -> None:
    fx.cancel_timer(ctx)
    fx.set_edp(True, ctx)


def _on_enter_docked(ctx: Context, fx: Effectors) -> None:
    fx.cancel_timer(ctx)
    fx.set_edp(False, ctx)


def _on_enter_deferred(ctx: Context, fx: Effectors) -> None:
    fx.cancel_timer(ctx)
    fx.set_edp(False, ctx)
    fx.pause_media()


def _on_enter_countdown(ctx: Context, fx: Effectors) -> None:
    fx.set_edp(False, ctx)
    fx.start_grace_timer(ctx)


def _on_enter_suspending(ctx: Context, fx: Effectors) -> None:
    fx.cancel_timer(ctx)


async def _suspending_tail(ctx: Context, fx: Effectors) -> None:
    """Lock-before-suspend: trigger lock, wait up to 2s, then suspend regardless."""
    if not ctx.locked:
        await fx.request_lock()
        engaged = await fx.wait_for_lock(ctx)
        if not engaged:
            LOG.warning(
                "lock did not engage in %.1fs — suspending anyway", LOCK_WAIT_SECONDS
            )
        else:
            LOG.info("lock engaged; proceeding to suspend")
    else:
        LOG.info("already locked; proceeding to suspend")
    await fx.do_suspend()


async def on_enter_screen(state: ScreenState, ctx: Context, fx: Effectors) -> None:
    if state is ScreenState.ACTIVE:
        fx.cancel_screen_timer(ctx)
        # Only call dpms on if we may have turned it off previously. Calling
        # dpms on when screens are already on is harmless but logs noise.
        fx.dpms(True)
    elif state is ScreenState.DIM_PENDING:
        fx.start_screen_timer(ctx)
    elif state is ScreenState.DIMMED:
        fx.cancel_screen_timer(ctx)
        fx.dpms(False)


# =========================================================================
# Daemon: pure transitions (Layer 3)
# =========================================================================


def desired_state(state: State, ev: EventKind, ctx: Context) -> State | None:
    if ev is EventKind.TIMER_EXPIRED:
        return State.SUSPENDING if state is State.COUNTDOWN else None

    if ev is EventKind.RESUMED:
        return _world_state(ctx) if state is State.SUSPENDING else None

    if state is State.SUSPENDING:
        return None

    target = _world_state(ctx)
    return target if target is not state else None


def _world_state(ctx: Context) -> State:
    if not ctx.lid_closed:
        return State.LID_OPEN
    if ctx.ext_mon_count >= 1:
        return State.DOCKED
    if ctx.inhibitor:
        return State.DEFERRED
    return State.COUNTDOWN


def desired_screen_state(
    main_state: State, screen_state: ScreenState, ev: EventKind, ctx: Context
) -> ScreenState | None:
    """Pure transition for the screen-DPMS sub-FSM.

    Active only when the main FSM is showing a screen (LID_OPEN or DOCKED).
    Otherwise force ACTIVE.

    DIM_PENDING → DIMMED is the one event-driven transition; everything else
    is computed from (locked, inhibitor) ∧ main_state.
    """
    if main_state not in (State.LID_OPEN, State.DOCKED):
        return ScreenState.ACTIVE if screen_state is not ScreenState.ACTIVE else None

    if ev is EventKind.SCREEN_TIMER_EXPIRED:
        return (
            ScreenState.DIMMED
            if screen_state is ScreenState.DIM_PENDING
            else None
        )

    target: ScreenState
    if not (ctx.locked and ctx.inhibitor):
        target = ScreenState.ACTIVE
    elif screen_state is ScreenState.DIMMED:
        target = ScreenState.DIMMED  # stay dimmed; only unlock/inhibit-off exits
    else:
        target = ScreenState.DIM_PENDING

    return target if target is not screen_state else None


# =========================================================================
# Helpers
# =========================================================================


def run(cmd: list[str], check: bool = True) -> subprocess.CompletedProcess:
    try:
        return subprocess.run(cmd, check=check, capture_output=True, text=True)
    except subprocess.CalledProcessError as e:
        LOG.warning("command failed: %s (rc=%d): %s", cmd, e.returncode, e.stderr.strip())
        if check:
            raise
        return e


def _edp_is_disabled() -> bool | None:
    try:
        out = run(["hyprctl", "monitors", "all", "-j"], check=False).stdout
        for m in json.loads(out):
            if m.get("name") == EDP_MONITOR:
                return bool(m.get("disabled", False))
    except Exception as e:
        LOG.warning("_edp_is_disabled failed: %s", e)
    return None


def _hyprctl_ext_monitor_count() -> int:
    try:
        out = run(["hyprctl", "-j", "monitors"]).stdout
        mons = json.loads(out)
        return sum(1 for m in mons if not m["name"].startswith("eDP"))
    except Exception as e:
        LOG.warning("ext_monitor_count failed: %s", e)
        return 0


def _wayland_inhibitor_active() -> bool:
    try:
        if not HYPRIDLE_LOG.exists():
            return False
        with HYPRIDLE_LOG.open("rb") as f:
            f.seek(0, 2)
            size = f.tell()
            f.seek(max(0, size - 8192))
            tail = f.read().decode("utf-8", errors="replace")
        latest = None
        for line in tail.splitlines():
            m = re.search(r"Inhibit locks:\s*(\d+)", line)
            if m:
                latest = int(m.group(1))
        return bool(latest and latest > 0)
    except Exception as e:
        LOG.warning("wayland inhibitor check failed: %s", e)
        return False


def _hyprlock_running() -> bool:
    return (
        subprocess.run(["pgrep", "-x", "hyprlock"], capture_output=True).returncode == 0
    )


def _read_on_ac_sysfs() -> bool | None:
    """Read on_ac from /sys/class/power_supply/AC*/online as a fallback for the
    UPower D-Bus subscription. Returns None on a desktop / no AC supply."""
    try:
        for ac in Path("/sys/class/power_supply").glob("A*/online"):
            try:
                return ac.read_text().strip() == "1"
            except OSError:
                continue
    except Exception:
        pass
    return None


async def _logind_real_inhibitor_active(manager_iface) -> bool:
    try:
        rows = await manager_iface.call_list_inhibitors()
    except Exception as e:
        LOG.warning("ListInhibitors failed: %s", e)
        return False
    for who, _why, what, mode, _uid, _pid in rows:
        if mode != "block":
            continue
        cats = (what or "").split(":")
        if "idle" not in cats and "sleep" not in cats:
            continue
        if who in INHIBIT_BASELINE_WHO:
            continue
        return True
    return False


def _wake_state_snapshot() -> dict[str, str]:
    """Read /sys/.../power/wakeup for each tracked device. Returns {label: state}."""
    out: dict[str, str] = {}
    try:
        if Path(WAKE_USB_CONTROLLER).exists():
            out["controller"] = Path(WAKE_USB_CONTROLLER).read_text().strip()
    except Exception:
        pass
    try:
        for hub in Path("/sys/bus/usb/devices").glob("usb*/power/wakeup"):
            try:
                out[hub.parent.parent.name] = hub.read_text().strip()
            except Exception:
                continue
    except Exception:
        pass
    try:
        for vendor_path in Path("/sys/bus/usb/devices").glob("*/idVendor"):
            try:
                v = vendor_path.read_text().strip()
                p_path = vendor_path.parent / "idProduct"
                if not p_path.exists():
                    continue
                p = p_path.read_text().strip()
                if (v, p) in WAKE_USB_VENDORS:
                    wake_path = vendor_path.parent / "power" / "wakeup"
                    if wake_path.exists():
                        out[WAKE_USB_VENDORS[(v, p)]] = wake_path.read_text().strip()
            except Exception:
                continue
    except Exception:
        pass
    return out


# =========================================================================
# Event sources
# =========================================================================


async def hypr_socket_reader(queue: asyncio.Queue, ctx: Context) -> None:
    sig = os.environ.get("HYPRLAND_INSTANCE_SIGNATURE")
    runtime = os.environ.get("XDG_RUNTIME_DIR")
    if not sig or not runtime:
        LOG.error("HYPRLAND_INSTANCE_SIGNATURE / XDG_RUNTIME_DIR not set")
        return
    sock_path = f"{runtime}/hypr/{sig}/.socket2.sock"

    while True:
        try:
            reader, _writer = await asyncio.open_unix_connection(sock_path)
            LOG.info("connected to Hyprland event socket %s", sock_path)
            while True:
                line_b = await reader.readline()
                if not line_b:
                    break
                line = line_b.decode(errors="replace").strip()
                ev = _parse_hypr_event(line)
                if ev is not None:
                    await queue.put(ev)
        except (FileNotFoundError, ConnectionRefusedError) as e:
            LOG.warning("hypr socket unavailable (%s); retrying in 2s", e)
            await asyncio.sleep(2)
        except Exception as e:
            LOG.exception("hypr socket reader crashed: %s", e)
            await asyncio.sleep(2)


def _parse_hypr_event(line: str) -> Event | None:
    # Monitor events fire for all outputs (including eDP). The dispatcher's
    # MONITOR_ADDED/REMOVED handler delegates to _hyprctl_ext_monitor_count
    # for the lid-FSM count (which itself filters eDP), and feeds
    # schedule_profile_reconcile which considers the full output set.
    if line.startswith(("monitoradded>>", "monitoraddedv2>>")):
        name = line.split(">>", 1)[1].split(",")[0]
        return Event(EventKind.MONITOR_ADDED, payload=name)
    elif line.startswith("monitorremoved>>"):
        name = line.split(">>", 1)[1]
        return Event(EventKind.MONITOR_REMOVED, payload=name)
    elif line.startswith("configreloaded"):
        return Event(EventKind.RECONCILE, payload="configreloaded")
    return None


async def inhibitor_poller(queue: asyncio.Queue, ctx: Context, manager_iface) -> None:
    last_logind = ctx.logind_inhibitor
    last_wayland = ctx.wayland_inhibitor
    while True:
        try:
            cur_logind = await _logind_real_inhibitor_active(manager_iface)
        except Exception as e:
            LOG.warning("logind inhibitor poll failed: %s", e)
            cur_logind = last_logind
        cur_wayland = _wayland_inhibitor_active()

        if cur_logind != last_logind:
            await queue.put(Event(
                EventKind.INHIBITOR_ON if cur_logind else EventKind.INHIBITOR_OFF,
                payload="logind",
            ))
            last_logind = cur_logind
        if cur_wayland != last_wayland:
            await queue.put(Event(
                EventKind.INHIBITOR_ON if cur_wayland else EventKind.INHIBITOR_OFF,
                payload="wayland",
            ))
            last_wayland = cur_wayland
        await asyncio.sleep(INHIBIT_POLL_SECONDS)


async def gpu_mode_poller(queue: asyncio.Queue) -> None:
    """Queue events when platform_profile or the gpu-select override changes.
    Sibling of inhibitor_poller (sysfs/file diff -> event); the reconciler
    stays event-free per its silent-ctx-repair contract."""
    last_profile = _read_first_word(PLATFORM_PROFILE_PATH)
    last_override = _read_first_word(GPU_OVERRIDE_FILE)
    while True:
        await asyncio.sleep(INHIBIT_POLL_SECONDS)
        cur = _read_first_word(PLATFORM_PROFILE_PATH)
        if cur != last_profile:
            last_profile = cur
            await queue.put(Event(EventKind.PLATFORM_PROFILE_CHANGED, payload=cur))
        cur = _read_first_word(GPU_OVERRIDE_FILE)
        if cur != last_override:
            last_override = cur
            await queue.put(Event(EventKind.GPU_OVERRIDE_CHANGED, payload=cur))


def _resolve_gpu_actual(ctx: Context) -> None:
    """Lazily resolve the session's actual device list. Stays pending on
    transient failure (Hyprland not up yet — daemon start races compositor
    exec) so the next drift check retries; the socket reader has the same
    retry posture."""
    devices = _hyprland_aq_devices()
    if devices is None:
        return  # still pending
    ctx.gpu_actual_pending = False
    if devices:
        ctx.gpu_actual = devices
        return
    # Var unset: normally an unmanaged session — except after a transient or
    # validation bail at select time, where the user still wants to hear that
    # a relog would now produce a managed selection.
    reason = None
    try:
        reason = json.loads(gpu_state_path().read_text()).get("reason")
    except (OSError, ValueError):
        pass
    ctx.gpu_actual = [] if reason in ("bailed-transient", "validation-failed") else None


def gpu_drift_check(ctx: Context, fx: Effectors, trigger: str,
                    profile_gpu: str) -> None:
    """Compute desired vs actual; notify on mismatch (debounced). Decision
    lives here (Layer 3 style); fx.notify_gpu_drift is the only effect.
    profile_gpu is always the FRESH select_profile result ("auto" on
    no-match), never ctx.current_profile — stale profiles must not drive
    advice."""
    if ctx.gpu_actual_pending:
        _resolve_gpu_actual(ctx)
    if ctx.gpu_actual_pending or ctx.gpu_actual is None:
        return  # not resolved yet, or unmanaged session
    mode, source = resolve_gpu_mode(profile_overlay=profile_gpu)
    desired, reason = gpu_desired(gpu_snapshot(), mode, source)
    if desired is None:
        return  # nothing actionable to advise
    if desired == ctx.gpu_actual:
        ctx.gpu_last_notified = None  # re-arm: future drift notifies again
        return
    key = ":".join(desired)
    if key == ctx.gpu_last_notified:
        return
    now = time.monotonic()
    if now - ctx.gpu_last_notify_at < GPU_NOTIFY_MIN_SECONDS:
        return
    ctx.gpu_last_notified = key
    ctx.gpu_last_notify_at = now
    fx.notify_gpu_drift(desired, reason, trigger, on_ac=ctx.on_ac)


async def reconciler(ctx: Context, fx: Effectors, manager_iface) -> None:
    """Every RECONCILE_SECONDS, refresh ctx + re-assert eDP and DPMS invariants."""
    while True:
        await asyncio.sleep(RECONCILE_SECONDS)
        try:
            real_lid = await manager_iface.get_lid_closed()
            real_ext = _hyprctl_ext_monitor_count()
            real_logind_inh = await _logind_real_inhibitor_active(manager_iface)
            real_wayland_inh = _wayland_inhibitor_active()
            real_locked = _hyprlock_running()
            real_on_ac = _read_on_ac_sysfs()
        except Exception as e:
            LOG.warning("reconciler snapshot failed: %s", e)
            continue

        drift = []
        if real_lid != ctx.lid_closed:
            drift.append(f"lid_closed {ctx.lid_closed}->{real_lid}")
            ctx.lid_closed = real_lid
        if real_ext != ctx.ext_mon_count:
            drift.append(f"ext_mon {ctx.ext_mon_count}->{real_ext}")
            ctx.ext_mon_count = real_ext
        if real_logind_inh != ctx.logind_inhibitor:
            drift.append(f"logind_inh {ctx.logind_inhibitor}->{real_logind_inh}")
            ctx.logind_inhibitor = real_logind_inh
        if real_wayland_inh != ctx.wayland_inhibitor:
            drift.append(f"wayland_inh {ctx.wayland_inhibitor}->{real_wayland_inh}")
            ctx.wayland_inhibitor = real_wayland_inh
        if real_locked != ctx.locked:
            drift.append(f"locked {ctx.locked}->{real_locked} (pgrep fallback)")
            ctx.locked = real_locked
        if real_on_ac is not None and real_on_ac != ctx.on_ac:
            drift.append(f"on_ac {ctx.on_ac}->{real_on_ac} (sysfs fallback)")
            ctx.on_ac = real_on_ac

        if drift:
            LOG.warning("reconciler ctx drift: %s", "; ".join(drift))

        if ctx.state is None or ctx.state is State.SUSPENDING:
            continue

        # eDP invariant. The profile's edp_policy can override the
        # lid-driven default, so the reconciler routes through set_edp(...,
        # ctx) and only logs drift when the *resolved* policy is being
        # violated.
        edp_disabled = _edp_is_disabled()
        should_be_enabled = ctx.state is State.LID_OPEN
        if ctx.edp_policy == "disable":
            should_be_enabled = False
        elif ctx.edp_policy == "enable":
            should_be_enabled = True
        if edp_disabled is not None:
            if should_be_enabled and edp_disabled:
                LOG.warning("reconciler: state=%s edp_policy=%s but eDP disabled — re-enabling",
                            ctx.state.value, ctx.edp_policy)
                fx.set_edp(True, ctx)
            elif (not should_be_enabled) and (not edp_disabled):
                LOG.warning("reconciler: state=%s edp_policy=%s but eDP enabled — re-disabling",
                            ctx.state.value, ctx.edp_policy)
                fx.set_edp(False, ctx)

        # DPMS-DIMMED invariant: re-issue dpms off (idempotent). hyprctl
        # doesn't expose per-monitor dpms status cleanly, so we just re-apply.
        if ctx.screen_state is ScreenState.DIMMED:
            fx.dpms(False)


async def setup_upower_watcher(bus, queue: asyncio.Queue, ctx: Context) -> None:
    """Subscribe to UPower's OnBattery property. Currently observed only —
    no transition consumes ctx.on_ac. Wired in so future power-aware behaviour
    has a clean signal to attach to.
    """
    try:
        introspect = await bus.introspect(UPOWER_BUS, UPOWER_PATH)
        obj = bus.get_proxy_object(UPOWER_BUS, UPOWER_PATH, introspect)
        props = obj.get_interface("org.freedesktop.DBus.Properties")
        upower = obj.get_interface(UPOWER_IFACE)
    except Exception as e:
        LOG.warning("UPower interface unavailable: %s — on_ac left at default", e)
        return

    try:
        ctx.on_ac = not bool(await upower.get_on_battery())
    except Exception as e:
        LOG.warning("initial OnBattery read failed: %s", e)

    def on_props_changed(iface: str, changed: dict, _invalidated: list):
        if iface != UPOWER_IFACE:
            return
        if "OnBattery" not in changed:
            return
        new_on_ac = not bool(changed["OnBattery"].value)
        if new_on_ac == ctx.on_ac:
            return
        ctx.on_ac = new_on_ac
        asyncio.create_task(queue.put(
            Event(EventKind.AC_PLUGGED if new_on_ac else EventKind.AC_UNPLUGGED)
        ))

    props.on_properties_changed(on_props_changed)
    LOG.info("subscribed to UPower OnBattery (on_ac=%s)", ctx.on_ac)


async def setup_logind_watchers(
    bus, manager_iface, queue: asyncio.Queue, ctx: Context
):
    """Subscribe to Manager.PrepareForSleep, Manager.PropertiesChanged (LidClosed),
    and Session.PropertiesChanged (LockedHint).

    The session-level subscription requires resolving the session path first
    via Manager.GetSessionByPID(0)."""
    introspect_mgr = await bus.introspect(LOGIND_BUS, LOGIND_PATH)
    obj_mgr = bus.get_proxy_object(LOGIND_BUS, LOGIND_PATH, introspect_mgr)
    props_mgr = obj_mgr.get_interface("org.freedesktop.DBus.Properties")
    mgr = obj_mgr.get_interface(LOGIND_IFACE)

    def on_prepare_for_sleep(started: bool):
        if not started:
            asyncio.create_task(queue.put(Event(EventKind.RESUMED)))

    mgr.on_prepare_for_sleep(on_prepare_for_sleep)

    def on_mgr_properties_changed(iface: str, changed: dict, _invalidated: list):
        if iface != LOGIND_IFACE:
            return
        if "LidClosed" in changed:
            v = changed["LidClosed"].value
            asyncio.create_task(queue.put(
                Event(EventKind.LID_CLOSE if v else EventKind.LID_OPEN)
            ))

    props_mgr.on_properties_changed(on_mgr_properties_changed)

    # ---- session-level subscription ----
    session_path: str | None = None
    try:
        session_path = await mgr.call_get_session_by_pid(0)
    except Exception as e:
        LOG.warning("GetSessionByPID(0) failed: %s — falling back to ListSessions", e)
        try:
            sessions = await mgr.call_list_sessions()
            uid = os.getuid()
            # ListSessions returns a(susso): (id, uid, user, seat, path).
            # Prefer the active graphical session (Class=user, Type != tty).
            # Falling back to the manager session (`user@1000.service`) won't
            # see hyprlock's LockedHint changes.
            best_path = None
            best_score = -1
            for _sid, suid, _user, _seat, path in sessions:
                if suid != uid:
                    continue
                try:
                    intr = await bus.introspect(LOGIND_BUS, path)
                    s_obj = bus.get_proxy_object(LOGIND_BUS, path, intr)
                    s_iface = s_obj.get_interface(SESSION_IFACE)
                    state = await s_iface.get_state()
                    s_class = await s_iface.get_class()
                    s_type = await s_iface.get_type()
                except Exception:
                    continue
                # Score: graphical user session > online > anything else.
                score = 0
                if s_class == "user":
                    score += 1
                if s_type in ("wayland", "x11", "mir"):
                    score += 2
                if state == "active":
                    score += 4
                elif state == "online":
                    score += 1
                if score > best_score:
                    best_score = score
                    best_path = path
            session_path = best_path
        except Exception as e2:
            LOG.warning("ListSessions fallback failed: %s", e2)
    if session_path is None:
        LOG.warning("no logind session resolved — lock detection via LockedHint disabled")
        return None

    introspect_sess = await bus.introspect(LOGIND_BUS, session_path)
    obj_sess = bus.get_proxy_object(LOGIND_BUS, session_path, introspect_sess)
    props_sess = obj_sess.get_interface("org.freedesktop.DBus.Properties")
    session = obj_sess.get_interface(SESSION_IFACE)

    def on_sess_properties_changed(iface: str, changed: dict, _invalidated: list):
        if iface != SESSION_IFACE:
            return
        if "LockedHint" in changed:
            v = bool(changed["LockedHint"].value)
            # Update ctx eagerly so on_enter_SUSPENDING's wait_for_lock can see
            # the change without waiting for the dispatcher to consume the event.
            ctx.locked = v
            asyncio.create_task(queue.put(
                Event(EventKind.LOCK_ENGAGED if v else EventKind.LOCK_RELEASED)
            ))

    props_sess.on_properties_changed(on_sess_properties_changed)
    LOG.info("subscribed to session %s for LockedHint changes", session_path)
    return session


# =========================================================================
# Dispatcher
# =========================================================================


async def dispatcher(
    queue: asyncio.Queue, ctx: Context, fx: Effectors, initial: State
) -> None:
    state = initial
    ctx.state = state
    LOG.info(
        "initial state: %s (ext_mon=%d, inhibitor=%s, locked=%s, on_ac=%s)",
        state.value, ctx.ext_mon_count, ctx.inhibitor, ctx.locked, ctx.on_ac,
    )
    await on_enter(state, ctx, fx)

    # Pre-evaluate sub-FSM in case we're starting in LID_OPEN/DOCKED already
    # locked + inhibited.
    new_screen = desired_screen_state(state, ctx.screen_state, EventKind.RECONCILE, ctx)
    if new_screen and new_screen is not ctx.screen_state:
        LOG.info("SCREEN: %s -> %s (initial)", ctx.screen_state.value, new_screen.value)
        ctx.screen_state = new_screen
        await on_enter_screen(new_screen, ctx, fx)

    while True:
        ev: Event = await queue.get()

        if ev.kind is EventKind.RECONCILE:
            if state is not State.SUSPENDING:
                LOG.info("RECONCILE (%s): re-asserting %s/%s",
                         ev.payload, state.value, ctx.screen_state.value)
                await on_enter(state, ctx, fx)
                await on_enter_screen(ctx.screen_state, ctx, fx)
            continue

        # ---- update ctx from event ----
        if ev.kind is EventKind.LID_CLOSE:
            ctx.lid_closed = True
        elif ev.kind is EventKind.LID_OPEN:
            ctx.lid_closed = False
        elif ev.kind in (EventKind.MONITOR_ADDED, EventKind.MONITOR_REMOVED):
            ctx.ext_mon_count = _hyprctl_ext_monitor_count()
            # Debounce: monitor changes often arrive in bursts (mode/scale
            # negotiation). Coalesce into a single MONITORS_CHANGED event
            # before reconciling the active profile.
            fx.schedule_profile_reconcile(ctx)
        elif ev.kind is EventKind.MONITORS_CHANGED:
            sig = monitor_signature()
            profiles = load_profiles()
            chosen = select_profile(sig, profiles)
            if chosen is None:
                LOG.info("PROFILE: no match for signature=%s (have %d profiles)",
                         sorted(sig), len(profiles))
            elif chosen.name != ctx.current_profile:
                fx.apply_profile(chosen, ctx)
            else:
                LOG.debug("PROFILE: signature change but %s still wins", chosen.name)
            # Breadcrumb before drift check: "relog to apply" must always be
            # satisfiable by one relog, so next-login select needs the same
            # profile overlay the drift computation is about to use.
            fx.write_gpu_breadcrumb(chosen.gpu if chosen else None)
            gpu_drift_check(ctx, fx, "monitors changed",
                            chosen.gpu if chosen else "auto")
            continue  # profile reconciliation does not feed the main FSM
        elif ev.kind is EventKind.INHIBITOR_ON:
            if ev.payload == "logind":
                ctx.logind_inhibitor = True
            elif ev.payload == "wayland":
                ctx.wayland_inhibitor = True
        elif ev.kind is EventKind.INHIBITOR_OFF:
            if ev.payload == "logind":
                ctx.logind_inhibitor = False
            elif ev.payload == "wayland":
                ctx.wayland_inhibitor = False
        # LOCK_ENGAGED/RELEASED already update ctx.locked in the
        # PropertiesChanged callback (eager).

        # AC_PLUGGED/AC_UNPLUGGED: ctx.on_ac was already set in the
        # PropertiesChanged callback. We log the event but no transition
        # currently consumes it.
        if ev.kind in (EventKind.AC_PLUGGED, EventKind.AC_UNPLUGGED):
            LOG.info("AC: %s (on_ac=%s)", ev.kind.value, ctx.on_ac)

        # GPU drift advice on power/override changes. NOT routed through
        # RECONCILE (its early `continue` above would skip this) — these
        # events fall through to the FSMs below, which ignore them.
        if ev.kind in (EventKind.AC_PLUGGED, EventKind.AC_UNPLUGGED,
                       EventKind.PLATFORM_PROFILE_CHANGED,
                       EventKind.GPU_OVERRIDE_CHANGED):
            chosen = select_profile(monitor_signature(), load_profiles())
            gpu_drift_check(ctx, fx, ev.kind.value,
                            chosen.gpu if chosen else "auto")

        # ---- main FSM ----
        new = desired_state(state, ev.kind, ctx)
        if new is not None and new is not state:
            LOG.info(
                "STATE: %s -> %s (event=%s, ext_mon=%d, inhibitor=%s, locked=%s, on_ac=%s)",
                state.value, new.value, ev.kind.value,
                ctx.ext_mon_count, ctx.inhibitor, ctx.locked, ctx.on_ac,
            )
            state = new
            ctx.state = state
            await on_enter(state, ctx, fx)
        else:
            LOG.debug(
                "ignored: %s in %s (ext_mon=%d, inhibitor=%s, locked=%s, on_ac=%s)",
                ev.kind.value, state.value,
                ctx.ext_mon_count, ctx.inhibitor, ctx.locked, ctx.on_ac,
            )

        # ---- sub-FSM ----
        new_screen = desired_screen_state(state, ctx.screen_state, ev.kind, ctx)
        if new_screen is not None and new_screen is not ctx.screen_state:
            LOG.info(
                "SCREEN: %s -> %s (event=%s, locked=%s, inhibitor=%s, main=%s)",
                ctx.screen_state.value, new_screen.value, ev.kind.value,
                ctx.locked, ctx.inhibitor, state.value,
            )
            ctx.screen_state = new_screen
            await on_enter_screen(new_screen, ctx, fx)


# =========================================================================
# Subcommand: daemon
# =========================================================================


async def daemon_main() -> None:
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(levelname)s %(message)s",
        stream=sys.stdout,
    )

    # Wake-state diagnostic (read-only — fix would require the sleep hook).
    wake = _wake_state_snapshot()
    if wake:
        line = " ".join(f"{k}={v}" for k, v in wake.items())
        LOG.info("usb-wake state: %s", line)
        bad = [k for k, v in wake.items() if v != "enabled"]
        if bad:
            LOG.warning("wake disabled on: %s — install sleep hook (`hyprstate install`)",
                        ", ".join(bad))
    else:
        LOG.info("usb-wake state: (none of the tracked devices found)")

    from dbus_next import BusType
    from dbus_next.aio import MessageBus

    queue: asyncio.Queue = asyncio.Queue()

    bus = await MessageBus(bus_type=BusType.SYSTEM, negotiate_unix_fd=True).connect()
    introspect = await bus.introspect(LOGIND_BUS, LOGIND_PATH)
    obj = bus.get_proxy_object(LOGIND_BUS, LOGIND_PATH, introspect)
    manager = obj.get_interface(LOGIND_IFACE)

    ctx = Context()

    # Lid inhibitor first.
    fx_partial = Effectors(bus, manager, None, queue)
    await fx_partial.take_lid_inhibitor()

    # Resolve session, subscribe, snapshot.
    session = await setup_logind_watchers(bus, manager, queue, ctx)
    await setup_upower_watcher(bus, queue, ctx)
    fx = Effectors(bus, manager, session, queue)
    fx._lid_inhibit_fd = fx_partial._lid_inhibit_fd

    ctx.lid_closed = await manager.get_lid_closed()
    ctx.logind_inhibitor = await _logind_real_inhibitor_active(manager)
    ctx.wayland_inhibitor = _wayland_inhibitor_active()
    ctx.ext_mon_count = _hyprctl_ext_monitor_count()

    # Seed the active profile from the existing .active.conf symlink (if any),
    # so subsequent reconciles can detect "no change" instead of forcing a
    # spurious hyprctl reload at startup. Then queue a one-shot
    # MONITORS_CHANGED to align with reality (covers profiles edited offline
    # or monitors plugged while the daemon was down).
    try:
        if ACTIVE_PROFILE_LINK.is_symlink():
            ctx.current_profile = ACTIVE_PROFILE_LINK.resolve().stem
            for p in load_profiles():
                if p.name == ctx.current_profile:
                    ctx.edp_policy = p.edp
                    break
    except OSError as e:
        LOG.warning("could not read %s: %s", ACTIVE_PROFILE_LINK, e)
    LOG.info("initial profile: %s (edp_policy=%s)",
             ctx.current_profile, ctx.edp_policy)
    await queue.put(Event(EventKind.MONITORS_CHANGED))

    if session is not None:
        try:
            ctx.locked = bool(await session.get_locked_hint())
        except Exception as e:
            LOG.warning("initial LockedHint read failed: %s", e)
            ctx.locked = _hyprlock_running()
    else:
        ctx.locked = _hyprlock_running()

    initial = _world_state(ctx)

    await asyncio.gather(
        dispatcher(queue, ctx, fx, initial),
        hypr_socket_reader(queue, ctx),
        inhibitor_poller(queue, ctx, manager),
        gpu_mode_poller(queue),
        reconciler(ctx, fx, manager),
    )


# =========================================================================
# Subcommand: sleep-hook
# =========================================================================


def sleep_hook_main(action: str) -> int:
    """Run as root from /usr/lib/systemd/system-sleep/. Maintains
    /sys/.../power/wakeup="enabled" on USB hubs and tracked input devices."""
    if action not in ("pre", "post"):
        return 0  # systemd-suspend may fire this with other actions; ignore.

    log_path = Path("/var/log/hyprstate-sleep.log")
    try:
        log_path.parent.mkdir(parents=True, exist_ok=True)
        log_fh = log_path.open("a")
    except OSError as e:
        sys.stderr.write(f"hyprstate sleep-hook: cannot open log: {e}\n")
        log_fh = sys.stderr

    def log(msg: str) -> None:
        from datetime import datetime
        log_fh.write(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] {msg}\n")
        log_fh.flush()

    label = "PRE-SUSPEND" if action == "pre" else "POST-RESUME"
    log(f"=== {label}: enabling USB wake ===")

    def write_enabled(path: Path) -> bool:
        try:
            path.write_text("enabled")
            return True
        except OSError as e:
            log(f"  ! {path}: {e}")
            return False

    # USB controller (PCI device).
    ctrl = Path(WAKE_USB_CONTROLLER)
    if ctrl.exists():
        ok = write_enabled(ctrl)
        log(f"  controller: {'enabled' if ok else 'FAILED'}")

    # USB root hubs.
    hubs = list(Path("/sys/bus/usb/devices").glob("usb*/power/wakeup"))
    enabled_hubs = sum(1 for h in hubs if write_enabled(h))
    log(f"  root hubs: {enabled_hubs}/{len(hubs)} enabled")

    # Intermediate hubs (devices whose product field contains "Hub").
    intermediate = 0
    for product in Path("/sys/bus/usb/devices").glob("*/product"):
        try:
            if "Hub" in product.read_text():
                wake = product.parent / "power" / "wakeup"
                if wake.exists() and write_enabled(wake):
                    intermediate += 1
        except OSError:
            continue
    log(f"  intermediate hubs: {intermediate} enabled")

    # Specific input devices.
    for vendor_path in Path("/sys/bus/usb/devices").glob("*/idVendor"):
        try:
            v = vendor_path.read_text().strip()
        except OSError:
            continue
        p_path = vendor_path.parent / "idProduct"
        if not p_path.exists():
            continue
        try:
            p = p_path.read_text().strip()
        except OSError:
            continue
        if (v, p) in WAKE_USB_VENDORS:
            wake = vendor_path.parent / "power" / "wakeup"
            if wake.exists():
                ok = write_enabled(wake)
                log(f"  {WAKE_USB_VENDORS[(v, p)]}: {'enabled' if ok else 'FAILED'}")

    log(f"=== {label} complete ===")
    if log_fh is not sys.stderr:
        log_fh.close()
    return 0


# =========================================================================
# Subcommand: install / uninstall / status
# =========================================================================


def install_main() -> int:
    """Delegate to install.sh living next to this script."""
    here = Path(__file__).resolve().parent
    script = here / "install.sh"
    if not script.exists():
        print(f"install.sh not found at {script}", file=sys.stderr)
        return 1
    os.execvp("bash", ["bash", str(script)])  # noqa: returns nonzero only on exec failure
    return 1


def uninstall_main() -> int:
    cmds = [
        ["systemctl", "--user", "disable", "--now", "hyprstate.service"],
        ["sudo", "rm", "-f", "/usr/local/bin/hyprstate",
         "/usr/lib/systemd/system-sleep/hyprstate"],
        ["rm", "-f", str(Path.home() / ".config/systemd/user/hyprstate.service")],
        ["systemctl", "--user", "daemon-reload"],
    ]
    rc = 0
    for cmd in cmds:
        print(f"+ {' '.join(cmd)}")
        try:
            subprocess.run(cmd, check=False)
        except Exception as e:
            print(f"  failed: {e}", file=sys.stderr)
            rc = 1
    return rc


def status_main() -> int:
    print("=== systemctl --user status hyprstate.service ===")
    subprocess.run(["systemctl", "--user", "status", "hyprstate.service",
                    "--no-pager"], check=False)
    print("\n=== last 20 log lines ===")
    subprocess.run(["journalctl", "--user", "-u", "hyprstate.service",
                    "-n", "20", "--no-pager"], check=False)
    print("\n=== logind handle-lid-switch inhibitor ===")
    subprocess.run(["systemd-inhibit", "--list", "--no-pager"], check=False)
    print("\n=== gpu selection ===")
    _gpu_status()
    return 0


# =========================================================================
# Subcommand: gpu
# =========================================================================


def gpu_main(action: str) -> int:
    """CLI: gpu select | check | status.

    select/check print the device list (or nothing) on stdout — consumed raw
    by uwsm's env-hyprland — so logging is stderr-only here (the daemon's
    stdout basicConfig convention is deliberately not used) and the list is
    the single, final stdout write."""
    logging.basicConfig(level=logging.WARNING,
                        format="%(levelname)s %(message)s", stream=sys.stderr)
    if action == "status":
        return _gpu_status()

    snap = gpu_snapshot()
    mode, source = resolve_gpu_mode()
    devices, reason = gpu_desired(snap, mode, source)
    if devices is None and reason == "bailed-transient" and action == "select":
        # One settle retry: docked cold boot can race DP link training.
        time.sleep(GPU_SETTLE_SECONDS)
        snap = gpu_snapshot()
        devices, reason = gpu_desired(snap, mode, source)
    if devices is not None and not all(os.path.exists(d) for d in devices):
        # All-or-nothing: dropping individual paths could silently violate
        # the integrated-always-included / usable-output invariants.
        devices, reason = None, "validation-failed"
    if action == "select":
        _write_gpu_state(mode, reason, devices, snap)
    if devices:
        sys.stdout.write(":".join(devices) + "\n")
        sys.stdout.flush()
    return 0


def _gpu_status() -> int:
    try:
        state = json.loads(gpu_state_path().read_text())
        print(f"intent : mode={state.get('mode')} reason={state.get('reason')}")
        print(f"         devices={':'.join(state.get('devices') or []) or '(none)'}")
    except (OSError, ValueError):
        print("intent : (no state file)")
    actual = _hyprland_aq_devices()
    if actual is None:
        print("actual : (no Hyprland session found)")
    elif not actual:
        print("actual : (compositor defaults — AQ_DRM_DEVICES unset)")
    else:
        print(f"actual : {':'.join(actual)}")
    mode, source = resolve_gpu_mode()
    desired, reason = gpu_desired(gpu_snapshot(), mode, source)
    if desired is None:
        print(f"desired: (unmanaged — {reason})")
    else:
        print(f"desired: {':'.join(desired)}  (mode={mode}/{source}, {reason})")
    if actual is not None and desired is not None:
        print("sync   : in sync" if desired == actual
              else "sync   : MISMATCH — relog to apply")
    return 0


def profile_main(action: str, name: str | None = None) -> int:
    """CLI: profile list | current | switch <name>.

    The daemon's MONITORS_CHANGED handler is the canonical apply path.
    `switch` repoints .active.conf and signals SIGHUP-equivalent by
    poking the daemon: it'll re-evaluate via `hyprctl reload` reaching the
    socket2 `configreloaded` event, then a synthesized MONITORS_CHANGED."""
    profiles = load_profiles()
    sig = monitor_signature()

    if action == "list":
        for p in sorted(profiles, key=lambda p: -p.priority):
            matches = ", ".join(p.matches)
            applies = "✓" if all(_match_in_signature(m, sig) for m in p.matches) else " "
            print(f"  [{applies}] {p.name:<28} prio={p.priority} edp={p.edp:<7} match=[{matches}]")
        return 0

    if action == "current":
        try:
            target = ACTIVE_PROFILE_LINK.resolve()
            print(target.stem if ACTIVE_PROFILE_LINK.is_symlink() else "(no active profile)")
        except OSError:
            print("(no active profile)")
        return 0

    if action == "switch":
        if not name:
            print("switch requires a profile name", file=sys.stderr)
            return 2
        match = next((p for p in profiles if p.name == name), None)
        if match is None:
            print(f"unknown profile: {name}", file=sys.stderr)
            print("available:", ", ".join(p.name for p in profiles), file=sys.stderr)
            return 1
        # Repoint the symlink atomically; the daemon's reconciler will pick
        # up the change on its next pass, but we also call hyprctl reload
        # directly so the user sees the effect immediately.
        try:
            tmp = ACTIVE_PROFILE_LINK.with_suffix(ACTIVE_PROFILE_LINK.suffix + ".tmp")
            if tmp.exists() or tmp.is_symlink():
                tmp.unlink()
            tmp.symlink_to(match.path)
            tmp.replace(ACTIVE_PROFILE_LINK)
        except OSError as e:
            print(f"symlink failed: {e}", file=sys.stderr)
            return 1
        subprocess.run(["hyprctl", "reload"], check=False)
        print(f"switched to {name}")
        return 0

    print(f"unknown action: {action}", file=sys.stderr)
    return 2


# =========================================================================
# Entrypoint
# =========================================================================


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="hyprstate")
    sub = parser.add_subparsers(dest="cmd", required=True)
    sub.add_parser("daemon", help="run the FSM (systemd --user)")
    p_hook = sub.add_parser("sleep-hook", help="invoked by systemd-suspend (root)")
    p_hook.add_argument("action", choices=["pre", "post"],
                        nargs="?", default=None)
    p_hook.add_argument("sleep_type", nargs="?", default=None)
    sub.add_parser("install", help="run install.sh")
    sub.add_parser("uninstall", help="reverse install")
    sub.add_parser("status", help="systemctl + journalctl summary")
    p_prof = sub.add_parser("profile", help="list / current / switch monitor profiles")
    p_prof.add_argument("action", choices=["list", "current", "switch"])
    p_prof.add_argument("name", nargs="?", default=None)
    p_gpu = sub.add_parser("gpu", help="GPU-primary selection (uwsm + drift status)")
    p_gpu.add_argument("action", choices=["select", "check", "status"])

    args = parser.parse_args(argv)
    if args.cmd == "daemon":
        try:
            asyncio.run(daemon_main())
        except KeyboardInterrupt:
            return 0
        return 0
    if args.cmd == "sleep-hook":
        # systemd-suspend invokes us with: <pre|post> <suspend|hibernate|...>.
        # argparse picked up the first positional as `action`.
        if args.action is None:
            print("sleep-hook requires pre|post", file=sys.stderr)
            return 1
        return sleep_hook_main(args.action)
    if args.cmd == "install":
        return install_main()
    if args.cmd == "uninstall":
        return uninstall_main()
    if args.cmd == "status":
        return status_main()
    if args.cmd == "profile":
        return profile_main(args.action, args.name)
    if args.cmd == "gpu":
        return gpu_main(args.action)
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
