# RPM spec for hyprstate (Rust v2). Built in COPR from a local SRPM
# produced by packaging/build-srpm.sh (source tarball from the git tag +
# vendored cargo deps as Source1 — no rust-*-devel packages needed).
# The test suite runs by default (cargo test over both workspace members).
# Disable for a one-off build with --without check; COPR builds run the suite.
%bcond_without check

Name:           hyprstate
Version:        2.7.2
Release:        1%{?dist}
Summary:        Hyprland session/power state machine (lid, monitors, profiles, GPU, powerd)
License:        MIT
URL:            https://github.com/MasonRhodesDev/hyprstate
Source0:        %{url}/archive/v%{version}/%{name}-%{version}.tar.gz
Source1:        %{name}-%{version}-vendor.tar.xz

BuildRequires:  cargo-rpm-macros >= 24
BuildRequires:  systemd-rpm-macros
# groupadd in %pre for the shared monitor-profiles group.
Requires(pre):  shadow-utils
Requires:       systemd
Requires:       dbus-common
%{?systemd_requires}
# Runtime conflicts with other platform_profile owners are handled by the
# powerd unit's Conflicts= line, NOT an RPM-level Conflicts: (p-p-d ships in
# the default Fedora install; a package conflict would make installs
# painful).
Recommends:     playerctl
Recommends:     hypridle

%description
Personal Hyprland session and power state machine for laptops: a user
daemon owning lid/suspend/lock/monitor-profile/GPU-drift/power policy, a
root powerd (org.hyprstate.Power1) applying sysfs power knobs, a
systemd-sleep hook keeping USB input devices wake-capable, and udev rules
for hotplugged hubs. Configuration lives in ~/.config/hypr (power.conf,
profiles/) and is not part of this package.

%prep
# -a1 unpacks the vendor tarball (vendor/ at its root) into the source dir.
%autosetup -p1 -a1
%cargo_prep -v vendor
# %%cargo_prep only redirects crates.io. Git pins are vendored in Source1
# too; map them so the RPM build stays offline.
cat >> .cargo/config.toml << 'EOF'

[source."git+https://github.com/MasonRhodesDev/monitor-profiles?rev=64d5d1e#64d5d1ed079582a2014ebf23c403a3ca03ee9c64"]
git = "https://github.com/MasonRhodesDev/monitor-profiles"
rev = "64d5d1e"
replace-with = "vendored-sources"
EOF

%build
%cargo_build
%{cargo_license_summary}
%{cargo_license} > LICENSE.dependencies

%install
# %%cargo_install re-resolves without Cargo.lock; git pins then fail offline.
# %%cargo_build already produced the rpm-profile binary.
install -Dpm0755 target/rpm/hyprstate %{buildroot}%{_bindir}/hyprstate
install -Dpm0644 dist/hyprstate.service %{buildroot}%{_userunitdir}/hyprstate.service
install -Dpm0644 dist/hyprstate-powerd.service %{buildroot}%{_unitdir}/hyprstate-powerd.service
install -Dpm0644 dist/org.hyprstate.Power1.conf %{buildroot}%{_datadir}/dbus-1/system.d/org.hyprstate.Power1.conf
install -Dpm0644 dist/org.hyprstate.Power1.service %{buildroot}%{_datadir}/dbus-1/system-services/org.hyprstate.Power1.service
install -Dpm0644 dist/60-hyprstate-usb-wake.rules %{buildroot}%{_udevrulesdir}/60-hyprstate-usb-wake.rules
install -Dpm0755 dist/sleep-hook-wrapper.sh %{buildroot}%{_prefix}/lib/systemd/system-sleep/hyprstate
install -Dpm0644 dist/90-hyprstate.system.preset %{buildroot}%{_presetdir}/90-hyprstate.preset
install -Dpm0644 dist/90-hyprstate.user.preset %{buildroot}%{_userpresetdir}/90-hyprstate.preset
install -d -m2775 %{buildroot}%{_sysconfdir}/monitor-profiles

%if %{with check}
%check
%cargo_test
%endif

%pre
# Shared monitor-profile group. The directory below is group-writable so a
# desktop user can edit layouts without root, and no username is ever baked
# in: the admin adds whoever should be able to. vigil creates the same group
# and co-owns the directory with identical attributes, so either package may
# be installed alone or both together.
getent group monitor-profiles >/dev/null || groupadd -r monitor-profiles || :

%post
%systemd_post hyprstate-powerd.service
%systemd_user_post hyprstate.service
# Load the freshly-installed udev rule and D-Bus policy now, so the USB-wake
# rule and powerd's name ownership work without waiting for a reboot.
%udev_rules_update
systemctl reload dbus-broker.service >/dev/null 2>&1 || systemctl reload dbus.service >/dev/null 2>&1 || :
if [ $1 -eq 1 ]; then
    # First install only: take exclusive ownership of platform_profile.
    # The systemd preset only covers the named unit, so the conflicting daemons
    # must be disabled explicitly or systemd may pick the wrong owner at boot
    # (both Conflicts= each other and would otherwise both be WantedBy multi-user).
    systemctl --quiet disable --now power-profiles-daemon.service tuned.service tlp.service >/dev/null 2>&1 || :
fi

%preun
%systemd_preun hyprstate-powerd.service
%systemd_user_preun hyprstate.service

%postun
%systemd_postun_with_restart hyprstate-powerd.service
%systemd_user_postun_with_restart hyprstate.service
%udev_rules_update

%files
%license LICENSE LICENSE.dependencies
%doc README.md POWER_SPEC.md GPU_SPEC.md
%{_bindir}/hyprstate
%{_unitdir}/hyprstate-powerd.service
%{_userunitdir}/hyprstate.service
%{_presetdir}/90-hyprstate.preset
%{_userpresetdir}/90-hyprstate.preset
%{_datadir}/dbus-1/system.d/org.hyprstate.Power1.conf
%{_datadir}/dbus-1/system-services/org.hyprstate.Power1.service
%{_udevrulesdir}/60-hyprstate-usb-wake.rules
%{_prefix}/lib/systemd/system-sleep/hyprstate
%dir %attr(2775,root,monitor-profiles) %{_sysconfdir}/monitor-profiles

%changelog
* Sun Sep 20 2026 Mason Rhodes <mrhodesdev@gmail.com> - 2.7.2-1
- Applying a monitor profile no longer runs `hyprctl reload`. It runs the
  rendered profile in the live Lua state (`hyprctl eval dofile(...)`), which
  only schedules a monitor-state refresh. A reload trips every refresh bit,
  including the blur-framebuffer pass that builds a framebuffer for every
  monitor and aborts on one still 0x0 mid-hotplug: Hyprland crashed this way
  on 2026-09-06 and 2026-09-20 when a monitor woke just before hyprstate's
  profile reload landed. If the eval is refused the daemon falls back to
  `reload`. The RECONCILE re-assert that used to ride on configreloaded now
  follows a successful apply directly.
* Thu Sep 04 2026 Mason Rhodes <mrhodesdev@gmail.com> - 2.7.1-1
- A refused Suspend() no longer wedges the FSM. logind rejecting the call
  (a masked suspend.target, transient trouble) used to park the daemon in
  SUSPENDING forever - only Resumed leaves that state, and no Resumed ever
  comes for a suspend that did not happen - leaving the machine locked but
  awake all night (2026-09-04 incident). do_suspend now reports the
  refusal and the transition is rejected, re-arming a fresh 30 s grace and
  retrying, the same fail-closed shape as an unengaged lock.
* Thu Sep 04 2026 Mason Rhodes <mrhodesdev@gmail.com> - 2.7.0-1
- The decided idle/power ladder (POWER_SPEC.md, 2026-09-04) in the FSM:
  a keep-awake claim governs only the UNLOCKED machine. WorldInputs gains
  locked and battery_low; an inhibitor on a locked machine defers nothing,
  and battery-low bypasses the claim gate outright (the daemon
  self-requests suspend on battery below the low threshold - the 30 s
  grace is the plug-in window; it still locks first).
- A standing suspend request now outranks Docked: docking neutralizes the
  lid as a suspend trigger but is not itself a keep-awake, so a genuinely
  idle docked laptop suspends like the desktop. Lid-close while docked
  still triggers nothing; lid-close mid-call (claim held, unlocked) still
  defers. hyprstate-fsm 5.0.0 (WorldInputs is a breaking change).
* Wed Sep 03 2026 Mason Rhodes <mrhodesdev@gmail.com> - 2.6.0-1
- Idle-suspend as a request into the existing lid Countdown machinery, and
  explicit lidless configuration. A lidless desktop could never suspend:
  the suspend route was reachable only via lid close, so the machine sat in
  LID_OPEN forever. A standing idle-suspend request (written by hypridle via
  `hyprstate suspend request`) now drives world_state to COUNTDOWN ahead of
  the lid chain -- but NOT ahead of DOCKED, so a docked laptop mid-work is
  still deliberately kept awake, exactly as before. `#@ lid = present|absent`
  in power.conf declares lid presence; absent skips the handle-lid-switch
  inhibitor (which otherwise showed on a desktop's `systemd-inhibit --list`)
  and disables the lid route. Every suspend still traverses the one lock
  proof before logind Suspend.

* Tue Aug 25 2026 Mason Rhodes <mrhodesdev@gmail.com> - 2.5.0-1
- Delete the screen-DPMS sub-FSM. hyprstate no longer blanks outputs at all:
  hypridle (hypr-DE >= 0.2.25) owns locked-screen blanking with an
  input-idle listener, which stops the 30 s re-blank under the user's hands
  after a wake (#24). The stuck-DPMS repair (DPMS on only) stays. Telemetry
  envelope v2 drops the `screen` field.

* Sat Aug 22 2026 Mason Rhodes <mrhodesdev@gmail.com> - 2.4.2-1
- Resolve hypr-ipc 0.1.1 so the old hypr-paths crate leaves the dependency graph.

* Sat Aug 22 2026 Mason Rhodes <mrhodesdev@gmail.com> - 2.4.1-1
- Depend on xdg-paths, logind-session, and hypr-ipc 0.1.1 (renamed crates, desktop-commons ADR 0005).

* Fri Aug 22 2026 Mason Rhodes <mrhodesdev@gmail.com> - 2.4.0-1
- Alpha cleanup: the hyprlang profile renderer, the per-profile dialect
  field, and `profile save --format` are removed. Renders and the active
  link are Lua only. ProfileFormat survives solely as the parse dialect of
  legacy hand-written files for `profile migrate`.

* Fri Aug 22 2026 Mason Rhodes <mrhodesdev@gmail.com> - 2.3.3-1
- Finish the Lua-only cut: only .active.lua is repointed (no stale .conf
  twin), renders are always .lua, profile save no longer writes .conf,
  and --format accepts only lua (kept for compatibility). Stale docs and
  the 2.3.0 changelog claim about .conf readability corrected: profiles
  load from TOML only; a leftover .conf render is untouched and
  `profile migrate` retires it.

* Fri Aug 22 2026 Mason Rhodes <mrhodesdev@gmail.com> - 2.3.2-1
- DIMMED policy reworked from review: per-output DPMS counts (a hotplugged
  panel lighting up next to blanked ones is re-asserted, not mistaken for a
  user wake), settle window anchored on the blank actually landing
  (DpmsApplied from the effector worker), a wake budget (2 per lock) so a
  panel that ignores DPMS cannot loop the screen FSM, no re-blank on
  config-reload reassert, shadow-gated observation events, and the verdict
  moved into the pure FSM (dimmed_action) with tests.
- Stale docs claiming hyprland.lua-existence dialect detection removed.

* Fri Aug 22 2026 Mason Rhodes <mrhodesdev@gmail.com> - 2.3.1-1
- DIMMED no longer re-blanks outputs every reconcile tick. With working
  Lua DPMS dispatch (2.3.0) that fought Hyprland's input wake
  (key_press/mouse_move_enables_dpms) and strobed the lock screen while
  the password was typed. A DPMS-on observation after a 3s settle now
  counts as user activity (ScreenWoken) and re-arms the dim timer.

* Fri Aug 22 2026 Mason Rhodes <mrhodesdev@gmail.com> - 2.3.0-1
- Lua-only: Hyprland 0.56 removed classic string dispatchers and the
  legacy config parser; hyprctl argv (dpms, eDP disable, workspace
  re-home) and profile rendering now always emit the Lua forms.
  The fragile hyprland.lua-existence dialect detection is gone (it broke
  DPMS the moment hypr-de-setup --adopt removed the home stub).
- Existing .conf profiles stay readable; saving migrates them to .lua.

* Wed Aug 19 2026 Mason Rhodes <mrhodesdev@gmail.com> - 2.2.4-1
- Share canonical monitor identity and profile selection with the desktop stack.
- Parse one typed Hyprland monitor snapshot per reconciliation cycle.

* Sun Aug 16 2026 Mason Rhodes <mrhodesdev@gmail.com> - 2.2.3-1
- Name the COPR project so Fedora publishes on tag.

* Sun Aug 16 2026 Mason Rhodes <mrhodesdev@gmail.com> - 2.2.2-1
- Snapshot Arch sources on tag builds so the PKGBUILD checksum can match.

* Sun Aug 16 2026 Mason Rhodes <mrhodesdev@gmail.com> - 2.2.1-1
- Pin hypr-paths, hypr-logind, and hypr-ipc to crates.io 0.1.0.
- Map monitor-profiles to the Cargo.lock git URL so COPR stays offline.

* Fri Aug 14 2026 Mason Rhodes <mrhodesdev@gmail.com> - 2.2.0-1
- Emit live Help telemetry and correctly detect idle inhibitors

* Thu Aug 13 2026 Mason Rhodes <mrhodesdev@gmail.com> - 2.1.8-1
- Hot-reload active profile when shared/user TOML changes (#19).

* Thu Aug 13 2026 Mason Rhodes <mrhodesdev@gmail.com> - 2.1.7-1
- Ignore generated lua/conf when warning about unmigrated legacy.

* Thu Aug 13 2026 Mason Rhodes <mrhodesdev@gmail.com> - 2.1.6-1
- TOML-only profile sources; use monitor-profiles::to_toml (#18).

* Thu Aug 13 2026 Mason Rhodes <mrhodesdev@gmail.com> - 2.1.5-1
- Ship shared monitor-profiles integration (#13) and post-merge fixes to the reference machine.

* Mon Aug 10 2026 Mason Rhodes <mrhodesdev@gmail.com> - 2.1.4-1
- Lock detection now relies solely on logind LockedHint (locker-agnostic:
  vigil-lock, hyprlock, swaylock, ...). The old pgrep hyprlock check was dead
  and its false reading clobbered real lock state every reconcile pass,
  keeping DPMS-off-when-locked+inhibited from ever engaging (#12)
- Repair a stuck DPMS-off state hypridle lost the wake for
- Reconciler runs one hyprctl per pass instead of four

* Wed Jul 22 2026 Mason Rhodes <mrhodesdev@gmail.com> - 2.1.3-1
- Re-home workspaces stranded on the disabled eDP: Hyprland only evacuates a
  disabled monitor's workspaces to a monitor enabled at disable time and never
  re-homes them when an external returns, so an undock flap pinned them to the
  dead panel. The daemon now moves them to an external on dock changes.

* Wed Jul 15 2026 Mason Rhodes <mrhodesdev@gmail.com> - 2.1.2-1
- Fix eDP disable and dpms effectors under the Hyprland Lua config (keyword
  is legacy-only; use eval / hl.dsp.dpms per dialect)
- Require the literal 'ok' hyprctl reply for mutations (exit code alone
  misses Lua-mode keyword rejection)
- Declarative eDP state marker (~/.config/hypr/edp-off) so config reloads
  converge instead of re-enabling the panel

* Fri Jul 03 2026 Mason Rhodes <mrhodesdev@gmail.com> - 2.0.2-1
- Standardized packaging release: shared CI, arch-repo + COPR pipeline

* Mon Jun 29 2026 Mason Rhodes <mrhodesdev@gmail.com> - 2.0.1-1
- Pin discrete-GPU runtime PM in dgpu mode (SetDgpuAwake) to prevent the
  Framework 16 D3cold/DCN resume wedge
- Drop the Python-era install.sh; packaged install only (RPM / PKGBUILD)

* Fri Jun 12 2026 Mason Rhodes <mrhodesdev@gmail.com> - 2.0.0-1
- Rust rewrite (v2): single binary, RPM-owned root paths replace the
  symlink dev install and the libexec privilege-boundary copy
