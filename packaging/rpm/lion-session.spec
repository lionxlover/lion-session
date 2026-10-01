# RPM spec for lion-session (Fedora / RHEL / SUSE families).

%bcond_without check

Name:           lion-session
Version:        0.3.0
Release:        1%{?dist}
Summary:        LionOS user session manager

License:        MIT
URL:            https://lionos.org/
Source0:        %{name}-%{version}.tar.gz

BuildRequires:  rust-packaging >= 21
BuildRequires:  dbus
BuildRequires:  python3
Requires:       dbus
%{?systemd_requires}

%description
Owns a logged-in desktop session end-to-end: environment setup,
any-compositor supervision with crash recovery, XDG + TOML autostart
with phases and per-app cgroup limits, the cooperative
end-of-session protocol, idle escalation into lock/logout,
lock-before-sleep and lock-on-shutdown, logind inhibitors, and a
versioned D-Bus surface (os.lionos.Session).

%prep
%autosetup -n %{name}-%{version}

%build
%cargo_build

%install
%cargo_install
install -Dpm 0644 systemd/lion-session.target %{buildroot}%{_unitdir}/lion-session.target
install -Dpm 0644 systemd/lion-session.service %{buildroot}%{_unitdir}/lion-session.service
install -Dpm 0644 dbus-1/services/os.lionos.Session.service \
    %{buildroot}%{_datadir}/dbus-1/services/os.lionos.Session.service
install -Dpm 0644 etc/lionos/session.toml %{buildroot}%{_sysconfdir}/lionos/session.toml

%check
%if %{with check}
%cargo_test
%endif

%files
%license LICENSE*
%doc README.md CHANGELOG.md STABILITY.md SECURITY.md
%{_bindir}/lion-session
%{_unitdir}/lion-session.target
%{_unitdir}/lion-session.service
%{_datadir}/dbus-1/services/os.lionos.Session.service
%config(noreplace) %{_sysconfdir}/lionos/session.toml

%changelog
* Wed Sep 30 2026 LionOS Project <packaging@lionos.org> - 0.3.0-1
- 0.3.0: idle escalation, lock-on-shutdown, process hardening,
  per-app cgroup limits, 5x faster name acquisition, D-Bus activation,
  orphan cleanup, maturity artifacts (CI/fuzz/packaging/contracts).
