# Changelog — lion-session

All notable changes, Keep-a-Changelog format, semver versions (see
STABILITY.md for what "breaking" means for a D-Bus surface and a
config schema).

## [0.3.0] — 2026-09-30

### Added — the gap-filling release
* **Idle escalation** (`[session] lock-after-ms` / `logout-after-ms`):
  the session manager itself locks after idle (anti-shoulder-surf
  default) and can end the session (kiosk). Pure state machine
  (`idle.rs`) driven by `SetIdle` + a 1 Hz tick; unconfigured = exact
  0.2.0 behaviour. New metrics `idle_locks` / `idle_logouts`.
* **Lock-on-shutdown** (default on): logind `PrepareForShutdown` now
  locks *before* fast-ending — a shutdown cancelled by another
  inhibitor leaves a locked session, not an exposed one. Metric
  `shutdown_locks`.
* **Process hardening** (`harden.rs`): non-dumpable (same-user
  processes cannot ptrace or read `/proc/<pid>/mem` of the manager),
  `RLIMIT_CORE=0`. NO_NEW_PRIVS deliberately absent (it would break
  sudo for every child); reasoning documented in-code. `--version`
  reports the applied level.
* **Orphan cleanup**: direct-launch children get
  `PR_SET_PDEATHSIG(SIGTERM)` — a SIGKILLed manager no longer leaks
  apps into the user's next session. Proven by test.
* **Per-app cgroup resource limits**: `memory-max` / `cpu-weight` /
  `tasks-max` in `session.toml` ride the systemd-run scopes
  (`--property=`), validated where config becomes argv; the direct
  fallback logs their inertness instead of failing.
* **Restart jitter** (compositor + apps): ±150 ms deterministic jitter
  prevents thundering-herd restart realignment after display hiccups.
* **D-Bus activation + user unit**: `systemd/lion-session.service`
  (Type=dbus, Restart=on-failure, hardened) and
  `dbus-1/services/os.lionos.Session.service` — the manager itself is
  now activatable and supervised like GNOME's.
* `--session <id>` argument (the greeter's 0.6.0 session choice).
* lib/bin split; deterministic fuzz harness
  (`tests/fuzz_harness.rs`); CI, packaging ×3, Makefile, CHANGELOG /
  STABILITY / SECURITY operator contracts.

### Changed — performance
* **Startup reordered**: the `os.lionos.Session` name is acquired
  before the compositor spawns, with the subprocess-heavy steps
  (activation-env import, systemd probe) moved behind it. Measured:
  **34–36 ms → 7–10 ms** to bus name (5×; the dbus-run-session floor
  is 4–5 ms).
* Bounded runtime: 2 worker threads + 4 blocking instead of
  `num_cpus()` workers (16 on typical hardware).

### Fixed — found by our own fuzz harness
* `exec_tokens` now excises NUL bytes from `Exec=` values: a crafted
  desktop file could otherwise produce a NUL in argv and abort the
  spawn (ENVALID-argument path).
* cgroup property values are validated (charset/length/weight range)
  before becoming `--property=` arguments.

## [0.2.0] — 2026-09-30
### Added
- 13-module rewrite: logind integration (delay inhibitors,
  PrepareForShutdown fast-end, lock-before-sleep, loginctl
  lock-session, idle hints), GSM-style cooperative end protocol with
  logout veto and bounded force, XDG autostart with the full lister
  rules, TOML autostart with phases and merge-by-name layering,
  compositor crash recovery (backoff + crash-loop guard, exit code
  3), systemd-run app scopes with auto fallback, sd_notify +
  watchdog, metrics surface, `--check-config`.

## [0.1.0] — initial delivery
- 860-line session starter (did not compile cleanly; 0.2.0 was the
  first working release — see TEST_REPORT for the baseline audit).
