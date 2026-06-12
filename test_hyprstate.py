"""Pure-layer tests: transition maps, profile selection/parsing, GPU
selection, and power policy. Nothing here touches D-Bus, hyprctl, or sysfs —
only the Layer-3 functions and the pure helpers they compose.

Run: pytest test_hyprstate.py
"""

import time

import pytest

import hyprstate as hs
from hyprstate import (
    Context,
    EventKind,
    GpuCard,
    GpuSnapshot,
    ScreenState,
    State,
    desired_screen_state,
    desired_state,
    gpu_desired,
    power_base_state,
    select_profile,
)


def ctx(**kw) -> Context:
    return Context(**kw)


# =========================================================================
# Main FSM: _world_state / desired_state
# =========================================================================


@pytest.mark.parametrize("c,expected", [
    (dict(lid_closed=False), State.LID_OPEN),
    (dict(lid_closed=False, ext_mon_count=2), State.LID_OPEN),
    (dict(lid_closed=True, ext_mon_count=1), State.DOCKED),
    (dict(lid_closed=True, logind_inhibitor=True), State.DEFERRED),
    (dict(lid_closed=True, wayland_inhibitor=True), State.DEFERRED),
    (dict(lid_closed=True), State.COUNTDOWN),
])
def test_world_state(c, expected):
    assert hs._world_state(ctx(**c)) is expected


def test_event_moves_to_world_state():
    c = ctx(lid_closed=True)
    assert desired_state(State.LID_OPEN, EventKind.LID_CLOSE, c) is State.COUNTDOWN


def test_same_state_is_none():
    c = ctx(lid_closed=False)
    assert desired_state(State.LID_OPEN, EventKind.RECONCILE, c) is None


def test_suspending_ignores_world_events():
    c = ctx(lid_closed=False)
    assert desired_state(State.SUSPENDING, EventKind.LID_OPEN, c) is None


def test_resumed_rederives_from_suspending_only():
    c = ctx(lid_closed=False)
    assert desired_state(State.SUSPENDING, EventKind.RESUMED, c) is State.LID_OPEN
    assert desired_state(State.LID_OPEN, EventKind.RESUMED, c) is None


def test_timer_expired_suspends_from_countdown():
    c = ctx(lid_closed=True)
    assert (desired_state(State.COUNTDOWN, EventKind.TIMER_EXPIRED, c)
            is State.SUSPENDING)


def test_timer_expired_ignored_outside_countdown():
    c = ctx(lid_closed=True)
    for state in (State.LID_OPEN, State.DOCKED, State.DEFERRED, State.SUSPENDING):
        assert desired_state(state, EventKind.TIMER_EXPIRED, c) is None


def test_stale_timer_must_not_suspend_lid_open_machine():
    """Regression: reconciler repaired lid_closed behind the FSM's back, so
    no transition cancelled the grace timer. Expiry must re-derive, not
    suspend."""
    c = ctx(lid_closed=False)
    assert (desired_state(State.COUNTDOWN, EventKind.TIMER_EXPIRED, c)
            is State.LID_OPEN)


def test_stale_timer_rederives_docked_and_deferred():
    assert (desired_state(State.COUNTDOWN, EventKind.TIMER_EXPIRED,
                          ctx(lid_closed=True, ext_mon_count=1)) is State.DOCKED)
    assert (desired_state(State.COUNTDOWN, EventKind.TIMER_EXPIRED,
                          ctx(lid_closed=True, logind_inhibitor=True))
            is State.DEFERRED)


def test_ctx_repaired_drives_transition():
    c = ctx(lid_closed=True)
    assert (desired_state(State.LID_OPEN, EventKind.CTX_REPAIRED, c)
            is State.COUNTDOWN)


# =========================================================================
# Screen sub-FSM
# =========================================================================


def test_screen_forced_active_when_no_screen_showing():
    c = ctx(locked=True, logind_inhibitor=True)
    assert (desired_screen_state(State.COUNTDOWN, ScreenState.DIMMED,
                                 EventKind.RECONCILE, c) is ScreenState.ACTIVE)
    assert (desired_screen_state(State.COUNTDOWN, ScreenState.ACTIVE,
                                 EventKind.RECONCILE, c) is None)


def test_screen_dim_pending_when_locked_and_inhibited():
    c = ctx(locked=True, wayland_inhibitor=True)
    assert (desired_screen_state(State.LID_OPEN, ScreenState.ACTIVE,
                                 EventKind.LOCK_ENGAGED, c)
            is ScreenState.DIM_PENDING)


def test_screen_timer_dims_only_from_dim_pending():
    c = ctx(locked=True, logind_inhibitor=True)
    assert (desired_screen_state(State.DOCKED, ScreenState.DIM_PENDING,
                                 EventKind.SCREEN_TIMER_EXPIRED, c)
            is ScreenState.DIMMED)
    assert (desired_screen_state(State.DOCKED, ScreenState.ACTIVE,
                                 EventKind.SCREEN_TIMER_EXPIRED, c) is None)


def test_screen_stays_dimmed_while_locked_and_inhibited():
    c = ctx(locked=True, logind_inhibitor=True)
    assert (desired_screen_state(State.LID_OPEN, ScreenState.DIMMED,
                                 EventKind.RECONCILE, c) is None)


def test_screen_wakes_on_unlock_or_inhibitor_release():
    assert (desired_screen_state(State.LID_OPEN, ScreenState.DIMMED,
                                 EventKind.LOCK_RELEASED,
                                 ctx(locked=False, logind_inhibitor=True))
            is ScreenState.ACTIVE)
    assert (desired_screen_state(State.LID_OPEN, ScreenState.DIMMED,
                                 EventKind.INHIBITOR_OFF, ctx(locked=True))
            is ScreenState.ACTIVE)


# =========================================================================
# Monitor profiles: parsing + selection
# =========================================================================


def write_profile(tmp_path, name, body):
    p = tmp_path / f"{name}.conf"
    p.write_text(body)
    return p


def test_parse_profile_full(tmp_path):
    p = write_profile(tmp_path, "desk", """\
#@ match = desc:Dell U2723QE
#@ match = desc:LG HDR 4K
#@ edp = disable
#@ gpu = dgpu
#@ hook = notify-send applied
#@ priority = 10
monitor = desc:Dell U2723QE, 3840x2160@60, 0x0, 1.5
""")
    prof = hs._parse_profile(p)
    assert prof.name == "desk"
    assert prof.matches == ("desc:Dell U2723QE", "desc:LG HDR 4K")
    assert prof.edp == "disable"
    assert prof.gpu == "dgpu"
    assert prof.hooks == ("notify-send applied",)
    assert prof.priority == 10


def test_parse_profile_default_priority_is_match_count(tmp_path):
    p = write_profile(tmp_path, "two", "#@ match = A\n#@ match = B\n")
    assert hs._parse_profile(p).priority == 2


def test_parse_profile_directives_stop_at_body(tmp_path):
    p = write_profile(tmp_path, "body", """\
#@ match = A
monitor = something
#@ edp = disable
""")
    assert hs._parse_profile(p).edp == "auto"  # post-body directive ignored


@pytest.mark.parametrize("body", [
    "monitor = no directives at all\n",          # no match
    "#@ match = A\n#@ edp = sideways\n",         # invalid edp
    "#@ match = A\n#@ gpu = both\n",             # invalid gpu
])
def test_malformed_profiles_are_skipped(tmp_path, body):
    write_profile(tmp_path, "bad", body)
    write_profile(tmp_path, "good", "#@ match = A\n")
    profiles = hs.load_profiles(tmp_path)
    assert [p.name for p in profiles] == ["good"]


def test_load_profiles_skips_dotfiles_and_missing_dir(tmp_path):
    write_profile(tmp_path, ".active", "#@ match = A\n")
    assert hs.load_profiles(tmp_path) == []
    assert hs.load_profiles(tmp_path / "nope") == []


def prof(name, matches, priority=None):
    return hs.Profile(name=name, path=None, matches=tuple(matches), edp="auto",
                      hooks=(), priority=priority if priority is not None
                      else len(matches))


def test_select_profile_requires_all_matches():
    sig = frozenset({"Dell U2723QE ABC123", "BOE 0x0BCA"})
    both = prof("both", ["Dell U2723QE", "BOE"])
    assert select_profile(sig, [both, prof("other", ["LG HDR 4K"])]) is both
    assert select_profile(frozenset({"BOE 0x0BCA"}), [both]) is None


def test_select_profile_specificity_then_explicit_priority():
    sig = frozenset({"Dell U2723QE", "LG HDR 4K"})
    one = prof("one", ["Dell U2723QE"])
    two = prof("two", ["Dell U2723QE", "LG HDR 4K"])
    assert select_profile(sig, [one, two]) is two  # more matches wins
    pinned = prof("pinned", ["LG HDR 4K"], priority=99)
    assert select_profile(sig, [one, two, pinned]) is pinned


def test_match_strips_desc_prefix_and_uses_startswith():
    sig = frozenset({"Dell U2723QE HJKL (DP-3)"})
    assert hs._match_in_signature("desc:Dell U2723QE", sig)
    assert hs._match_in_signature("Dell U2723QE", sig)
    assert not hs._match_in_signature("U2723QE", sig)  # not a prefix


# =========================================================================
# GPU selection
# =========================================================================


def card(path, card_n, boot_vga=0, vram=0, external=0, edp=0):
    return GpuCard(path=path, card=card_n, boot_vga=boot_vga, vram=vram,
                   external=external, edp=edp)


def fw16(igpu_edp=1, dgpu_external=0):
    """Framework-16-shaped snapshot: iGPU has boot_vga + small VRAM."""
    return (
        card("/dev/dri/by-path/pci-0000:03:00.0-card", "card1",
             vram=8 << 30, external=dgpu_external),
        card("/dev/dri/by-path/pci-0000:c4:00.0-card", "card2",
             boot_vga=1, vram=512 << 20, edp=igpu_edp),
    )


def snap(cards, non_pci=False, lid_closed=False):
    return GpuSnapshot(cards, non_pci, lid_closed)


def test_integrated_by_agreement():
    cards = fw16()
    assert hs._integrated_card(cards) is cards[1]


def test_integrated_disagreement_is_none():
    # boot_vga on the big-VRAM card (muxed laptop): signals disagree.
    cards = (card("a", "card0", boot_vga=1, vram=8 << 30),
             card("b", "card1", vram=512 << 20))
    assert hs._integrated_card(cards) is None


def test_integrated_vram_only_when_no_boot_vga():
    cards = (card("a", "card0", vram=8 << 30), card("b", "card1", vram=1 << 20))
    assert hs._integrated_card(cards) is cards[1]


def test_integrated_vram_tie_without_boot_vga_is_none():
    cards = (card("a", "card0"), card("b", "card1"))  # both vram=0 (Intel-style)
    assert hs._integrated_card(cards) is None


def test_gpu_unmanaged_cases():
    assert gpu_desired(snap(fw16()), "off", "override") == (None, "override-off")
    assert gpu_desired(snap(fw16()[:1]), "auto", "default") == (None, "no-multi-gpu")
    assert (gpu_desired(snap(fw16(), non_pci=True), "auto", "default")
            == (None, "non-pci-display-present"))
    ambiguous = (card("a", "card0", boot_vga=1, vram=8 << 30),
                 card("b", "card1", vram=1 << 20))
    assert (gpu_desired(snap(ambiguous), "auto", "default")
            == (None, "ambiguous-integrated"))


def test_gpu_auto_dgpu_with_display_is_primary():
    devices, reason = gpu_desired(snap(fw16(dgpu_external=1)), "auto", "default")
    assert devices == ["/dev/dri/card1", "/dev/dri/card2"]
    assert reason == "dgpu-has-display"


def test_gpu_auto_idle_dgpu_is_omitted():
    devices, reason = gpu_desired(snap(fw16(dgpu_external=0)), "auto", "default")
    assert devices == ["/dev/dri/card2"]  # iGPU only; dGPU omitted -> runtime PM
    assert reason == "dgpu-idle-omitted"


def test_gpu_igpu_mode_lists_dgpu_only_with_display():
    devices, reason = gpu_desired(snap(fw16(dgpu_external=1)), "igpu", "override")
    assert devices == ["/dev/dri/card2", "/dev/dri/card1"]
    assert reason == "override-igpu"
    devices, _ = gpu_desired(snap(fw16(dgpu_external=0)), "igpu", "override")
    assert devices == ["/dev/dri/card2"]


def test_gpu_dgpu_mode_always_lists_dgpu():
    devices, reason = gpu_desired(snap(fw16(dgpu_external=0)), "dgpu", "profile")
    assert devices == ["/dev/dri/card1", "/dev/dri/card2"]
    assert reason == "profile-dgpu"


def test_gpu_lid_closed_no_externals_bails_transient():
    devices, reason = gpu_desired(snap(fw16(), lid_closed=True), "auto", "default")
    assert devices is None and reason == "bailed-transient"


def test_gpu_usable_output_invariant():
    # Lid open but eDP not connected and no externals anywhere: any list we
    # could emit lights nothing up -> must bail, never print.
    devices, reason = gpu_desired(
        snap(fw16(igpu_edp=0, dgpu_external=0)), "auto", "default")
    assert devices is None and reason == "bailed-transient"


# =========================================================================
# Power policy
# =========================================================================


def test_power_base_state_ac_side():
    assert power_base_state(ctx(on_ac_settled=True)) == "ac"
    assert power_base_state(ctx(on_ac_settled=True, ext_mon_count=2)) == "docked-ac"


def test_power_base_state_battery_side():
    assert power_base_state(ctx(on_ac_settled=False, battery_percent=42.0)) == "battery"
    assert power_base_state(ctx(on_ac_settled=False, battery_percent=10.0,
                                low_battery=True)) == "battery-low"


def test_power_base_state_upower_down_on_battery():
    """Regression: battery_percent=None (UPower down) must not pin the axis
    to AC — the reconciler's sysfs on_ac repair has to reach battery
    profiles."""
    c = ctx(on_ac_settled=False, battery_percent=None)
    assert power_base_state(c) == "battery"


def test_power_base_state_desktop_stays_ac():
    # Desktops never observe an unplug: on_ac_settled stays True.
    c = ctx(on_ac_settled=True, battery_percent=None)
    assert power_base_state(c) == "ac"


def test_ac_axis():
    assert hs._ac_axis("ac") == "ac"
    assert hs._ac_axis("docked-ac") == "ac"
    assert hs._ac_axis("battery") == "battery"
    assert hs._ac_axis("battery-low") == "battery"


def test_profile_from_platform_value():
    assert hs.profile_from_platform_value("low-power") == "power-saver"
    assert hs.profile_from_platform_value("quiet") == "power-saver"
    assert hs.profile_from_platform_value("performance") == "performance"
    assert hs.profile_from_platform_value("balanced") == "balanced"
    assert hs.profile_from_platform_value(None) == "balanced"
    assert hs.profile_from_platform_value("custom") == "balanced"


def test_power_self_write_matches_full_fallback_chain():
    c = ctx()
    c.power_expected.append((time.monotonic() + 5,
                             frozenset(hs.PLATFORM_PROFILE_CHAINS["power-saver"])))
    # quiet-only firmware: we asked for power-saver, kernel wrote "quiet".
    assert hs.power_self_write(c, "quiet")
    assert c.power_expected == []  # entry consumed


def test_power_self_write_external_value_outside_window():
    c = ctx()
    c.power_apply_at = time.monotonic() - 100
    assert not hs.power_self_write(c, "performance")


def test_power_self_write_suppression_window():
    c = ctx()
    c.power_apply_at = time.monotonic()
    assert hs.power_self_write(c, "performance")  # any value inside the window


def test_power_self_write_prunes_expired_entries():
    c = ctx()
    c.power_apply_at = time.monotonic() - 100
    c.power_expected.append((time.monotonic() - 1, frozenset({"low-power"})))
    assert not hs.power_self_write(c, "low-power")
    assert c.power_expected == []


def test_load_power_policy_defaults_when_missing(tmp_path, monkeypatch):
    monkeypatch.setattr(hs, "POWER_CONF_FILE", tmp_path / "absent.conf")
    policy, pct = hs.load_power_policy()
    assert policy == hs.DEFAULT_POWER_POLICY
    assert pct == hs.DEFAULT_BATTERY_LOW_PCT


def test_load_power_policy_parses_and_validates(tmp_path, monkeypatch):
    conf = tmp_path / "power.conf"
    conf.write_text("""\
#@ docked-ac = performance
#@ battery = power-saver
#@ ac = warp-speed
#@ battery-low-percent = 80
#@ mystery-knob = 7
""")
    monkeypatch.setattr(hs, "POWER_CONF_FILE", conf)
    policy, pct = hs.load_power_policy()
    assert policy["docked-ac"] == "performance"
    assert policy["battery"] == "power-saver"
    assert policy["ac"] == "balanced"      # invalid value -> default kept
    assert pct == 50                       # clamped to [1, 50]
