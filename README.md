# lion-session

LionOS user session daemon, started by `lion-greeter` as the
authenticated user inside an already-open logind/PAM session. One small
Rust binary that owns the whole session lifecycle: environment, session
bus, compositor (with crash recovery), autostart (TOML + the XDG
cross-desktop standard), the cooperative end-of-session protocol, and
the lock/power bridge.

## What it does
- Prepares `XDG_*`, `WAYLAND_DISPLAY` and the session D-Bus bus
  (uses `dbus-user-session`'s socket-activated bus when present)
- Starts the compositor and waits for its Wayland socket; **respawns it
  with backoff when it crashes**, and refuses to steal a live socket
- Starts configured autostart apps **plus `/etc/xdg/autostart` /
  `~/.config/autostart` desktop entries** (Hidden / OnlyShowIn /
  NotShowIn / TryExec / X-GNOME-Autostart-enabled all honored), in
  startup phases, restarting per policy with crash-loop guards
- Serves `os.lionos.Session` on the session bus (full API below)
- Integrates with **logind**: delay inhibitors held while a session
  ends, lock-before-suspend, `loginctl lock-session`, idle hints,
  fast-end on `PrepareForShutdown` — and **locks on an imminent
  shutdown** (0.3.0: a *cancelled* shutdown then leaves a locked
  session, not an exposed one)
- **Escalates idle into lock/logout** (0.3.0): `[session]
  lock-after-ms` / `logout-after-ms` — the anti-shoulder-surf lock
  default and the kiosk auto-logout, owned by the manager itself
- **Hardens itself** (0.3.0): non-dumpable (no ptrace/`/proc/<pid>/mem`
  by same-user processes), no core dumps, and direct children carry
  PDEATHSIG so a SIGKILLed manager leaks no orphans
- **Per-app cgroup resource limits** (0.3.0): `memory-max` /
  `cpu-weight` / `tasks-max` ride the systemd-run scopes
- Acquires its bus name **before** the compositor spawns (0.3.0):
  7–10 ms to `os.lionos.Session` (was 34–36 ms)
- Forwards `Lock`/`Unlock` to `os.lionos.Locker` and power actions to
  `os.lionos.Power`
- Optionally launches autostart apps in `systemd-run --user --scope`
  (own cgroup, visible in `systemctl --user`), GNOME-style, with
  automatic fallback to direct children
- sd_notify READY/STATUS/WATCHDOG when started under systemd
- Reports metrics (`GetMetrics()`) in the same JSON dialect as
  lion-greeter

## What it deliberately does not do
- Render anything (the compositor's job)
- Authenticate users (`lion-greeter`'s job)
- Detect idle at the input-device level (`lion-idle` reports idle *to*
  this daemon via `SetIdle`; this daemon owns the *policy* — the 0.3.0
  division of labour)

## D-Bus API — `os.lionos.Session` @ `/os/lionos/Session` (iface `os.lionos.Session1`)

| Method | Meaning |
|---|---|
| `Logout()` / `Restart()` / `Shutdown()` | end the session cooperatively (see protocol below) |
| `Suspend()` / `Hibernate()` | forward to lion-power; session keeps running |
| `Lock()` / `Unlock()` | forward to lion-locker |
| `RegisterClient(app_id) -> u` | join the end-of-session protocol |
| `UnregisterClient(token) -> b` | leave it |
| `EndSessionResponse(token, ok, msg) -> s` | answer a QueryEndSession; `ok=false` **vetoes a logout** |
| `Inhibit(app_id, reason) -> u` / `Uninhibit(cookie) -> b` | delay an end while saving state |
| `IsInhibited() -> b` / `ListInhibitors() -> a(ss)` | inspect inhibitors |
| `SetIdle(idle: b)` | push idle hint to logind (`loginctl` sees it) |
| `GetCapabilities() -> as` | feature discovery |
| `GetMetrics() -> s` | JSON telemetry snapshot |

Signals: `QueryEndSession(s)`, `EndSession(s)`, `EndCanceled(s)`,
`PreparingToEnd(s)`, `IdleChanged(b)`, `InhibitorAdded(u,s,s)`,
`InhibitorRemoved(u)`, `CompositorRestarted(u)`.
Property (read-only): `State (s)` = `running` | `query-end` | `ending`,
announced via standard `PropertiesChanged`.

### The end-of-session protocol
1. `Logout/Restart/Shutdown` returns immediately; registered clients and
   apps holding inhibitors receive `QueryEndSession(reason)` ("save now").
2. Clients answer `EndSessionResponse(token, ok, message)`.
   An `ok=false` answer **vetoes a logout** (GNOME semantics) — the session
   emits `EndCanceled` and keeps running. Power actions cannot be vetoed;
   they are forced after the timeout.
3. When everyone answered (or `[session] end-timeout-ms` elapsed), the
   session emits `EndSession` and `PreparingToEnd` (the shell fades out),
   takes a **logind delay inhibitor** so the machine cannot finish
   powering off underneath the teardown, tears everything down in
   reverse order, and releases the inhibitor.

Divergence from GNOME, on purpose: our inhibitor wait is *bounded* —
a wedged app can never hang logout forever.

## Config
`/etc/lionos/session.toml`, overlaid by `~/.config/lionos/session.toml`
(`$LION_SESSION_CONFIG` replaces both). A broken file is logged and
ignored — it can never stop someone from logging in.
`lion-session --check-config` validates without starting a session.
See `etc/lionos/session.toml` for every documented default.

## Exit codes
- `0` clean end (any reason)
- `1` fatal setup error (no runtime dir, compositor never ready, ...)
- `3` compositor crash-loop guard tripped

## Files
- `src/` — the daemon (15 modules, zero new dependencies vs 0.1.0;
  lib + thin binary since 0.3.0)
- `tests/integration.rs` — end-to-end tests: mock logind, mock
  locker/power, fake compositor/app, real binary, real D-Bus
  (4 tests, 28 named CHECKs)
- `tests/fuzz_harness.rs` — deterministic fuzzing of the config
  layering and Exec tokenizer (thousands of mutants per run)
- `scripts/perf_probe.sh` — on-machine startup/RSS measurements
- `scripts/live_session_test.sh` — unit + integration + perf in one
  command
- `systemd/lion-session.target` — systemd --user target (app group)
- `systemd/lion-session.service` — activation/restart user unit (0.3.0)
- `dbus-1/services/os.lionos.Session.service` — D-Bus activation (0.3.0)
- `wayland-sessions/lionos.desktop` — session entry for display managers
- `etc/lionos/session.toml` — documented defaults
- `packaging/` — PKGBUILD, debian/, rpm/ (0.3.0)
- `Makefile`, `.github/workflows/ci.yml` — build/check/install, CI
- `CHANGELOG.md`, `STABILITY.md`, `SECURITY.md` — operator contracts
  (0.3.0)

## Security notes
- Refuses to run as root; validates `$XDG_RUNTIME_DIR` ownership and
  0700 mode (a shared runtime dir would leak the Wayland socket)
- **Process hardening (0.3.0)**: `PR_SET_DUMPABLE=0` (same-user
  processes cannot ptrace the manager or read its `/proc` entries),
  `RLIMIT_CORE=0`; `--version` reports the applied level.
  `PR_SET_NO_NEW_PRIVS` is deliberately absent — it is inherited by
  children and would break `sudo` for every app; see `SECURITY.md`.
- **Orphan cleanup (0.3.0)**: direct-launch children carry
  `PR_SET_PDEATHSIG(SIGTERM)`; a SIGKILLed manager leaks no apps.
- **Untrusted inputs**: desktop-file `Exec=` lines and TOML values are
  parsed/tokenized by rules whose invariants (no NUL into argv, no
  invalid cgroup properties, no panics) are fuzz-enforced in CI.
- Never decides authentication or authorization (greeter/polkit's job)
- logind absence, broken config, dead locker/power peers: all degrade
  to logged no-ops, never to a broken desktop
- No secrets are handled, so no mlock (unlike lion-greeter's password
  paths) — documented decision

## Version
0.3.0 — idle escalation (lock/logout), lock-on-shutdown, process
hardening + orphan cleanup, per-app cgroup limits, restart jitter,
D-Bus activation + user unit, 5× faster name acquisition, lib/bin
split, fuzz harness, CI, packaging, CHANGELOG/STABILITY/SECURITY.

0.2.0 — logind integration, cooperative end protocol (clients +
inhibitors + veto), XDG autostart, compositor crash recovery, systemd
scopes, sd_notify, metrics, SIGTERM grace, config `--check-config`.

0.1.0 — initial delivery (did not compile; see TEST_REPORT.md for the
baseline audit that 0.2.0 fixed).
