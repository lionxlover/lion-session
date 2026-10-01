# lion-session 0.2.0 — Test Report

## Summary

| Suite | Count | Result |
|---|---|---|
| Unit tests (`cargo test --bin lion-session`) | 44 | all pass, 0.03 s |
| Integration tests (`cargo test --test integration`) | 3 tests / 25 named checks | all pass, ~2.2 s |
| Static gates: fmt / clippy --all-targets | — | clean / 0 warnings |
| Performance probe (3 runs) | 9 measurements | 34–36 ms to IPC, 5.2–5.4 MB RSS |
| Live script (`scripts/live_session_test.sh`) | one command reproduces all of the above | green |

## Defects found by the tests during development (and fixed)

1. **0.1.0 did not compile** (missing `UserExt` import, 2× E0599).
2. **Shipped config silently ignored** — kebab-case TOML keys vs
   snake_case serde fields; the entire `etc/lionos/session.toml` was a
   no-op. Fixed + pinned by tests that use the shipped spellings.
3. **Stale-socket race** — a leftover `wayland-1` socket satisfied the
   old existence check instantly. Fixed with connect-probe (live →
   refuse, dead → remove); exercised by the crash-recovery test.
4. **SIGTERM = child massacre** — default handler left grandchildren
   unsignalled. Fixed with graceful fast-path teardown.
5. Two protocol races caught by the integration suite itself: the veto
   reply string is emitted before the cancel task runs (assertion moved
   to a property poll), and `wait_answers` initially did not wake on
   inhibitor release (added notify + test).

## Integration coverage (mock logind + mock locker/power + fake compositor)

Test 1 — `full_lifecycle_with_logind_mock` (17 checks):
name ownership; Wayland socket; compositor alive; capabilities
advertising every integration; RegisterClient/Inhibit/IsInhibited/
ListInhibitors; SetIdle → logind SetIdleHint; **PrepareForSleep(true) →
lock before suspend**; Lock/Unlock forwarding; Suspend without session
death; metrics counting the app; Shutdown (reply < 2 ms measured
1.8–1.9 ms) → power forward + **logind delay inhibitor taken** +
QueryEndSession; EndSessionResponse(ok) + Uninhibit; clean exit 0;
graceful app termination; compositor teardown; **inhibitor fd released
(EOF on the mock's pipe)**; EndSession/PreparingToEnd/InhibitorAdded
signals observed by an external subscriber.

Test 2 — `compositor_crash_recovery_and_crashloop_exit` (4 checks):
SIGKILL the compositor → respawn (new pid, session survives); stale
socket cleaned and recreated; second crash → **exit code 3**;
crash-loop visible in logs.

Test 3 — `logout_veto_and_forced_timeout` (4 checks):
`ok=false` veto → EndCanceled + state property returns to `running` +
session continues; approved logout ends the session; unanswered query
forced after the configured timeout (~0.93 s measured against a 0.6 s
budget — bounded below 10 s hard cap); forced end visible in logs.

## Unit test map (44)

- `config` (8): merge-by-name; session-options field layering;
  kebab-case spelling (the 0.1.0 defect); unknown keys tolerated;
  sort order (phase, delay, name); phase names; compositor recovery
  defaults; via default.
- `desktop` (11): group selection; locale fallback; Hidden; GNOME
  disable flag; OnlyShowIn/NotShowIn; Type gate; Exec quoting + escapes
  + field codes; app mapping; supervision flag; no-Exec rejection;
  TryExec miss; user-overrides-system by filename.
- `inhibit` (10): unique ids; query needed/not; veto short-circuit;
  all-answered; idempotent responses; timeout with pending list;
  inhibitor release unblocks wait; late/unknown answers; cookie
  lifecycle; sorted snapshots.
- `env` (3): runtime-dir ownership; mode 0700 enforcement (incl.
  setuid-bit tolerance); error messages that name the problem.
- `metrics` (3): snapshot completeness; counter independence;
  name/order alignment.
- `sdnotify` (3): WATCHDOG_USEC parsing; no-socket no-op; abstract
  name-length guard.
- `proc` (2): unit-name slug; in_path behavior.
- `bus` (1): address parsing.
- Misc (3): compositor timeouts, defaults, capabilities.

## Performance evidence (see OPTIMIZATIONS.md for the full table)

- 34–36 ms start → bus name; 37–40 ms → Wayland socket (python fake);
  5.2–5.4 MB RSS; exit code 0 through the public Logout() API via
  busctl from outside the process (an extra live check in the probe);
  dbus-run-session floor 4–5 ms; exec control 1 ms.

## Reproducing

```
cargo test                       # 44 unit
cargo test --test integration    # 3 tests / 25 checks
bash scripts/live_session_test.sh  # everything, verbose
bash scripts/perf_probe.sh         # measurements
```

No network, no root, no manual steps. Environment: Linux, Rust 1.98,
dbus-daemon, python3 (for the fake peers only).

---

# Round 3 (0.3.0) verification

## Command matrix

| Gate | Result |
|---|---|
| `cargo check` | 0 warnings |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo fmt --check` | clean |
| Unit tests | **58/58** (was 44) |
| Integration (real binary, private bus, mock peers) | **4/4** — 28 named CHECKs (was 25) |
| Fuzz harness | **4/4** (~14,000 mutants/run) |
| Live suite (`scripts/live_session_test.sh`) | all green in one command |
| Perf probe (3 runs) | bus-name **7–10 ms** (was 34–36), RSS 5.3–5.6 MB |

## New coverage, per feature

- **Idle escalation**: 8 unit tests on the state machine (disabled
  policy inert, lock-then-logout ordering, wake-cancels, single lock
  per idle period, kiosk-without-lock, terminal-after-end,
  no-double-lock) + integration test 4 (SetIdle → hint forwarded →
  lock fires after the configured timeout → wake resets → re-idle
  locks again → exactly-once assertion → metrics `idle_locks`:1).
- **Lock-on-shutdown**: integration CHECK 05/06 — PrepareForShutdown
  forwards Lock to the locker *before* the fast end; metric
  `shutdown_locks`; exit code 0.
- **Process hardening**: combined process-global test (parallel-runner
  safe): apply() → readback PR_GET_DUMPABLE=0, RLIMIT_CORE=0, tag
  consistency, dumpability round-trip.
- **Orphan cleanup (PDEATHSIG)**: spawns a real `sleep` on a helper
  thread that then exits; the child must not survive its parent
  thread.
- **Per-app resource limits**: config parse test (three properties →
  three `--property=` args; unconfigured → none) + fuzz-informed
  validation (invalid values skipped, NUL impossible, weight range
  enforced).
- **Startup reorder**: the perf probe itself (name before compositor:
  the socket-exists probe now measures "socket ready before name",
  proving the order flipped), plus integration CHECK 01 unchanged
  (name ownership).
- **Fuzz findings fixed**: Exec tokenizer NUL-excising (unit-tested
  via the harness invariants); cgroup property validation.
- **Config layering**: idle keys merge field-by-field like every
  other `[session]` key (unit test), `--check-config` prints the idle
  policy.

## Honest environment limits

- No real logind in this sandbox: shutdown/sleep paths run against
  the mock logind (the same limitation as 0.2.0, disclosed there).
- systemd-run scopes: unavailable here (no systemd user manager), so
  the scope path is exercised via the argument-construction tests and
  the documented fallback; the direct path is exercised live.
- RSS is reported from `ps` at steady state; ±0.3 MB run-to-run noise
  is normal on this kernel.
