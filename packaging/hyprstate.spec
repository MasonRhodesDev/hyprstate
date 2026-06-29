# RPM spec for hyprstate (Rust v2). Built in COPR from a local SRPM
# produced by packaging/build-srpm.sh (source tarball from the git tag +
# vendored cargo deps as Source1 — no rust-*-devel packages needed).
%bcond_with check

Name:           hyprstate
Version:        2.0.1
Release:        1%{?dist}
Summary:        Hyprland session/power state machine (lid, monitors, profiles, GPU, powerd)
License:        MIT
URL:            https://github.com/MasonRhodesDev/hyprstate
Source0:        %{url}/archive/v%{version}/%{name}-%{version}.tar.gz
Source1:        %{name}-%{version}-vendor.tar.xz

BuildRequires:  cargo-rpm-macros >= 24
BuildRequires:  systemd-rpm-macros
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

%build
%cargo_build
%{cargo_license_summary}
%{cargo_license} > LICENSE.dependencies

%install
%cargo_install
install -Dpm0644 dist/hyprstate.service %{buildroot}%{_userunitdir}/hyprstate.service
install -Dpm0644 dist/hyprstate-powerd.service %{buildroot}%{_unitdir}/hyprstate-powerd.service
install -Dpm0644 dist/org.hyprstate.Power1.conf %{buildroot}%{_datadir}/dbus-1/system.d/org.hyprstate.Power1.conf
install -Dpm0644 dist/org.hyprstate.Power1.service %{buildroot}%{_datadir}/dbus-1/system-services/org.hyprstate.Power1.service
install -Dpm0644 dist/60-hyprstate-usb-wake.rules %{buildroot}%{_udevrulesdir}/60-hyprstate-usb-wake.rules
install -Dpm0755 dist/sleep-hook-wrapper.sh %{buildroot}%{_prefix}/lib/systemd/system-sleep/hyprstate
install -Dpm0644 dist/90-hyprstate.system.preset %{buildroot}%{_presetdir}/90-hyprstate.preset
install -Dpm0644 dist/90-hyprstate.user.preset %{buildroot}%{_userpresetdir}/90-hyprstate.preset

%if %{with check}
%check
%cargo_test
%endif

%post
%systemd_post hyprstate-powerd.service
%systemd_user_post hyprstate.service

%preun
%systemd_preun hyprstate-powerd.service
%systemd_user_preun hyprstate.service

%postun
%systemd_postun_with_restart hyprstate-powerd.service
%systemd_user_postun_with_restart hyprstate.service

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

%changelog
* Mon Jun 29 2026 Mason Rhodes <mrhodesdev@gmail.com> - 2.0.1-1
- Pin discrete-GPU runtime PM in dgpu mode (SetDgpuAwake) to prevent the
  Framework 16 D3cold/DCN resume wedge
- Drop the Python-era install.sh; packaged install only (RPM / PKGBUILD)

* Fri Jun 12 2026 Mason Rhodes <mrhodesdev@gmail.com> - 2.0.0-1
- Rust rewrite (v2): single binary, RPM-owned root paths replace the
  symlink dev install and the libexec privilege-boundary copy
