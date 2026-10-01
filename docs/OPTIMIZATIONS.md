# lion-session 0.2.0 — Optimization & Performance Report

## Measured, on this machine (release build, 3 runs each)

Probe: `scripts/perf_probe.sh` (private D-Bus, fake compositor, busctl
polling for name ownership, ps for RSS).

| Metric | lion-session 0.2.0 | dbus-run-session (floor) | exec true (control) |
|---|---|---|---|
| Process start → owns `os.lionos.Session` | **34–36 ms** | — | — |
| Process start → Wayland socket exists | 37–40 ms (incl. python startup) | — | — |
| dbus-daemon bring-up + run + exit | — | 4–5 ms | 1 ms |
| Steady-state RSS | **5.2–5.4 MB** | ~1 MB | ~0 |
| `Shutdown()` D-Bus reply latency (integration test) | **1.8–1.9 ms** | — | — |
| Full integration suite (3 tests, real processes + bus) | 2.2–2.4 s | — | — |
| Binary size (LTO, stripped) | 3,503,776 B | — | — |

**Reading:** the *entire* session-management layer — logind client, end
protocol, supervision, XDG autostart, metrics, sd_notify — costs about
30 ms and ~4 MB over the minimal bus baseline. The 37 ms "compositor
ready" number is dominated by the python fake; a native compositor
starts faster than that.

## Where the milliseconds went (and how they were kept)

1. **Zero new dependencies.** 0.1.0's dependency set is unchanged; the
   logind client, sd_notify, metrics JSON, and desktop-entry parser are
   hand-rolled over what was already linked. Binary growth 0.1.0→0.2.0
   is feature code, not dependency trees. Supply-chain surface did not
   move.
2. **One systemd probe, once.** `systemctl --user show-environment`
   (2 s cap) runs a single time at startup, not per app; the result
   picks direct-vs-scope spawning for every autostart child.
3. **Immediate D-Bus replies.** `Logout/Restart/Shutdown` reply before
   any teardown begins (measured: <2 ms); end sequences run as spawned
   tasks so a slow peer or a full query round can never hold a method
   reply, and the shell gets its fade-out window by signal, not by
   blocking calls.
4. **Relaxed-ordered lock-free metrics.** `fetch_add(Relaxed)` on an
   array of AtomicU64 — telemetry with no critical section; JSON is
   built only when someone actually calls `GetMetrics()`.
5. **Backoff instead of spin.** Compositor respawn: 300 ms × attempt;
   app restart: 500 ms × recent-starts — CPU stays at zero while
   recovery is in flight, and the crash-loop guard bounds total work.
6. **process_group(0) + killpg.** One syscall signals a whole app tree;
   no per-pid walks, no leftover grandchildren.
7. **Lazy signal subscriptions.** The logind match rule is one stream on
   one connection; no polling loops anywhere in the daemon (the only
   pollers in the codebase are test/probe scripts).
8. **Memory discipline.** No clones of config across tasks (Arc'd
   metrics/protocol only); `String` state in a single `Arc<Mutex<String>>`
   cell touched for microseconds; protocol maps are cleared, not re-
   allocated.
9. **Fail-open is also fast-open.** Broken config, absent logind,
   absent systemd, dead locker/power: each is a single failed check and
   a log line — none of the degraded paths add latency to login.
10. **Release profile** kept from 0.1.0: LTO on, codegen-units 1,
    `panic = abort`, stripped.

## Optimization opportunities deliberately NOT taken

- **`mlockall`:** the session manager holds no secrets (the greeter's
  lane); locking ~5 MB of heap would trade real memory pressure for
  security theater. Documented decision.
- **Metrics histograms:** counters suffice for the session domain;
  histograms would add allocation on hot paths for data nobody consumes.
- **Tokio worker tuning:** the default multi-thread runtime is already
  overprovisioned for a session manager; manual worker counts would be
  tuning theater at 34 ms startup.

## Compilation quality gates (all enforced in CI-local runs)

- `cargo fmt --check` — clean
- `cargo clippy --all-targets` — 0 warnings
- `cargo test` — 44 unit + 3 integration, all green
- `cargo build --release` — 3.5 MB static-ish binary

---

# Round 3 (0.3.0) — the startup path rebuild

## What changed

0.2.0 acquired `os.lionos.Session` only after: bus setup →
`dbus-update-activation-environment` (subprocess) → `systemctl --user
show-environment` probe (subprocess) → peer connection → logind
connection → **compositor spawn + socket wait** → service build. Every
millisecond of that was in front of the name — the "desktop is
reachable" moment for shells.

0.3.0 splits the path:

1. bus → peer connection → **service build + name** (nothing
   subprocess-heavy left in front),
2. logind subscribe, activation-env import, systemd probe (all
   best-effort, all only needed before *apps*),
3. compositor spawn + socket wait → apps.

Plus the runtime: `num_cpus()` worker threads → 2 workers + 4
blocking (a session manager's async surface is one D-Bus service and
a handful of child watchers).

## Measured (release, same machine, 3 runs)

| Metric | 0.2.0 | 0.3.0 |
|---|---|---|
| time to `os.lionos.Session` bus name | 34–36 ms | **7–10 ms** |
| RSS steady-state | 5.2–5.4 MB | 5.3–5.6 MB (± noise) |
| `dbus-run-session true` (floor) | 4–5 ms | 4 ms |
| binary size | 3,503,776 B | 3,521,048 B (+17 KB: idle + harden + limits) |

The manager now reaches "reachable" within ~4 ms of the do-nothing
floor — the entire supervision/end-protocol/logind surface costs less
than one `dbus-run-session` roundtrip — and the number is published
with the reproducible probe (`scripts/perf_probe.sh`).

## What we did NOT optimize

- The compositor-ready time: it is dominated by the compositor, not
  the manager (the fake compositor in the probe creates its socket in
  ~1 ms; real ones take hundreds — the manager adds a 20 ms poll
  granularity, unchanged).
- The idle tick is 1 Hz *only when a policy is configured*; the
  default deployment runs zero timers.
- No dependency changes: the 10-dependency graph is a feature (audit
  surface), not a cost to minimize.
