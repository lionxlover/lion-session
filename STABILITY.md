# API stability policy — lion-session

The session manager sits between the greeter (which execs it), the
shell and apps (which call its D-Bus surface), and packagers (who
own its units). This is the contract those consumers develop against.

## Versioning

Semver, with the compatibility units defined below. Until 1.0.0 the
minor version is the breaking slot; from 1.0.0 standard semver.

## Stability tiers

### Tier 1 — D-Bus surface (`os.lionos.Session1`)

* **Never removed/renamed/re-typed once declared stable at 1.0.**
  Removal only via a deprecation cycle (docs → dual-path release →
  removal, each step in CHANGELOG.md).
* **Additions are not breaking**: `GetCapabilities()` exists so
  consumers feature-detect instead of guessing. A shell written for
  0.2.0 must keep working against 0.3.0 — and does: 0.3.0 only ADDS
  capabilities and signals.
* Stable-for-1.0 surface: `Logout` `Restart` `Shutdown` `Suspend`
  `Hibernate` `Lock` `Unlock` `RegisterClient` `UnregisterClient`
  `EndSessionResponse` `Inhibit` `Uninhibit` `IsInhibited`
  `ListInhibitors` `SetIdle` `GetCapabilities` `GetMetrics`; signals
  `QueryEndSession` `EndSession` `EndCanceled` `PreparingToEnd`
  `IdleChanged` `InhibitorAdded` `InhibitorRemoved`
  `CompositorRestarted`; property `State`.

### Tier 2 — CLI, config, units

* CLI flags (`--user`, `--session`, `--check-config`, `--version`)
  and exit codes (0 clean / 1 fatal / 3 compositor crash-loop) are
  contract.
* `session.toml` keys keep their meaning; unknown keys are ignored
  forever; a missing file is the default state. NEW: keys added in
  0.3.0 (`lock-after-ms`, `logout-after-ms`,
  `lock-on-shutdown`, per-app `memory-max`/`cpu-weight`/`tasks-max`)
  default to 0.2.0 behaviour — upgrading is always a no-op for
  existing deployments.
* The systemd user units and the D-Bus activation file are
  packaging-owned; their names (`lion-session.target`,
  `lion-session.service`, `os.lionos.Session.service`) are contract.

### Tier 3 — library API (unstable)

The `lion_session` library target (0.3.0) exists for tests and
first-party tooling; any item may change. Promote via an issue if an
external consumer needs a symbol pinned.

## How a release proves compatibility

The integration suite IS the consumer matrix: a release that would
break Tier 1/2 cannot pass `cargo test`/`scripts/live_session_test.sh`,
because those tests play the shell, the apps, logind, the locker and
the power daemon against the real binary.
