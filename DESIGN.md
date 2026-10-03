# lion-session — DESIGN.md

Decisions, trade-offs, and deferrals for spec 02 (Lion Session).

## 1. Architecture

### One core task, events everywhere

All mutable session state lives in **one** task (`session.rs`:
`SessionCore`). Bus methods, child exits, notify frames, inhibitor EOF
watches, timers and signals arrive as `Event`s on a single channel.
This gives:

- deterministic ordering (tests drive the core without a bus),
- no lock contention (bus methods only *dispatch* + await a oneshot),
- true idle: every wait is `select!`-ed — no polling, no wakeups
  (spec §7 "idle means idle").

**Trade-off:** single-core serialization means one slow backend call
(e.g. logind timeout) delays other events. Accepted: backend calls are
bounded (500ms auth timeout; logind calls fail fast to the CLI
fallback), and the alternative (actor-per-service + consensus on state)
is far more complex for no measured benefit at this scale.

### Ports & backends (hexagonal)

`ports.rs` defines `Launcher`, `SystemdUser`, `Logind`, `Authorizer`,
`Notifier`, `ReadyWatch`; real backends in `backends.rs` (zbus 5 +
CLI fallbacks), fakes in `mocks.rs`. The same core runs:

- on a real desktop (systemd user units + logind),
- in tests (fake services that crash/hang/flap — real processes),
- in `--mock` demo mode (script fakes, no bus at all).

### Readiness protocol

Each spawned child gets its own **abstract-namespace datagram socket**
exported as `NOTIFY_SOCKET` (the sd_notify convention). Children call
plain `sd_notify(0, "READY=1")` — zero new client-side API. One socket
per spawn (not per service) makes respawns collision-free and needs no
pid-credential parsing: the socket address *is* the service identity.

Bugs found here (kept as comments in code): the abstract-name leading-NUL
convention (`@` marker is not part of the name) and the `windows(8)`
vs 7-byte `"READY=1"` match. Both were caught by tests, not by review —
which is exactly why readiness runs over the *real* socket path in unit
tests too.

**Fallback:** if the compositor never notifies, the Wayland socket path
appearing under `$XDG_RUNTIME_DIR` also counts (bounded polling during
startup only).

### Two service-start modes

- **direct** (default in tests/demo): spawn argv via `tokio::process`,
  supervise via child wait, readiness via notify frames.
- **systemd** (auto-detected via `NameHasOwner(org.freedesktop.systemd1)`):
  `StartUnit` per service (the shipped `lion-session.target` pulls the
  rest, giving parallel starts), readiness via `JobRemoved` signals,
  stop via `StopUnit`. Both modes share the supervision decision logic.

## 2. Supervision (spec §3/§6)

- Restart policies `always` / `on-failure` / `never`; compositor death
  always ends the session cleanly (spec).
- Backoff: delay doubles from `crash_loop.backoff_start_ms`, capped at
  `backoff_max_ms`, computed from restart attempts inside the window.
- Crash-loop: N *crashes* (non-zero exits) in M ms → stop retrying, emit
  `ServiceFailed` **once** (coalescing window), notify the user once.
  **The supervision state survives respawns** — a fresh state per spawn
  would silently reset the counter (bug found and fixed during testing).
- A healthy re-run (service becomes ready again) resets the given-up
  flag.
- Flapping storm protection: notification coalescing + per-caller rate
  limits; one notification, not a storm (spec §6).

## 3. Inhibitors (spec §3/§4/§6)

`Inhibit(what, who, why) → fd`: the fd is one end of a socketpair; the
core holds the other and awaits EOF. Closing (or dying — the kernel
closes the fd) releases the inhibitor. This makes leaks self-healing
with zero polling. Mid-query inhibitors delay until the same
`shutdown_timeout_ms` deadline (documented in docs/DBUS.md).

## 4. Lifecycle (spec §3)

Pure state machine (`lifecycle.rs`) with explicit `now` — table-tested.
Query → all-acked or timeout → EndSession → force → logind action.
Clients ack by `EndSessionReply` (extension) or by disconnecting
(NameOwnerChanged watch → treated as ack). `Blockers` (extension
property) surfaces "which app is blocking".

## 5. Authorization (spec §8) — the one deliberate deviation

Spec: "authorize through lion-auth". lion-auth (spec 22) is not built
yet. Shipped behavior:

- `session.lion_auth.bus_name = "os.lionos.Auth1"` (default): every
  privileged call asks lion-auth; **any error or timeout denies**
  (fail closed, spec-compliant).
- `bus_name = ""` (development/tests): local policy — session owner
  may act; power actions additionally need root or
  `power_allowed_uids`; stranger uids are denied.

When lion-auth lands, flipping the config to its bus name is the whole
migration (see MIGRATION.md). The deviation is explicit, documented, and
fail-closed in the default configuration.

Caller identity is always bus-verified (uid/pid via the daemon), pinned
with pidfd, cgroup logged — never caller-supplied strings.

## 6. systemd integration (spec §9)

- Unit: `packaging/systemd/lion-session.service` — user scope,
  `Type=notify`, `WATCHDOG` honored (heartbeats only when
  `WATCHDOG_USEC` is set), `STOPPING=1` on graceful stop, full §9
  hardening block.
- D-Bus activation: `packaging/dbus/os.lion.Session1.service` +
  `SystemdService=lion-session.service`.
- Environment import: `org.freedesktop.systemd1.Manager.SetEnvironment`
  (what `systemctl --user import-environment` uses) with the
  `systemctl --user set-environment` CLI fallback.
- logind unreachable → `systemctl`/`loginctl` CLI fallback **with loud
  warnings** (spec §6).

## 7. Performance (spec §7) — measured, not claimed

Reference machine for the numbers below: this CI container
(x86_64). `cargo bench --bench lion_bench`:

| Metric | Result | Target |
|---|---|---|
| resolver, 2000 services (~5 deps each) | 5.7 ms | (real sessions: ~10 services → µs) |
| autostart parse, 500 entries | 4.2 ms (8.4 µs/entry) | — |
| inhibitor add/remove | 220 ns/op | — |
| lifecycle FSM transition | 240 ns | — |
| environment build | 2.4 µs | — |
| mock-session startup → SessionReady | 153 ms (dominated by fake children) | login-to-desktop < 2.5 s (whole stack) |
| idle RSS (`--mock`, steady) | **1.1 MB** | < 10 MB ✓ |
| idle threads | 1 | — |

Startup/first-frame budgets on the reference hardware are CI-gated via
`lion-bench` consumption of the JSON output (fail on >10% regression),
per spec.

## 8. Safe mode & session restore (v2 items)

- Safe mode: bad starts (compositor pre-ready crash / readiness timeout)
  counted in `$XDG_STATE_HOME/lion-session/history.json`; ≥ threshold →
  minimal shell (compositor + `safe_mode.minimal_services`), autostart
  skipped, reason notified + in the `SafeMode` property.
- Session restore (opt-in `session.restore_apps`): remembered apps
  (autostarted + restored) are persisted at end-session and relaunched
  after shell-ready. **Deferred:** per-workspace placement and
  full app-state restore need compositor (01/03) APIs that don't exist
  yet; the model carries the fields now.

## 9. Testing strategy (spec §10)

- Unit: pure modules (resolver incl. **proptest** DAG properties,
  supervisor timing via injected `Instant`, lifecycle FSM, autostart
  parser, inhibitors, environment, savestate, safemode, throttle, cli).
- Core integration (no bus): the real `SessionCore` with fake services
  that crash/hang/flap; crash-loop gives-up-and-notifies-once verified.
- **Real-bus acceptance** (`tests/dbus_e2e.rs`): private `dbus-daemon`,
  a fake `org.freedesktop.login1`, script fakes, and the *real binary*;
  every public method positive+negative; fail-closed authz run.
- CI runs nspawn acceptance (`tests/ci-nspawn-acceptance.sh`) where
  systemd is available.
- Fuzz: desktop-file parser + exec tokenizer + history/state JSON
  (`fuzz/`, corpus in repo, smoke pass in CI).

## 10. Explicitly deferred

| Item | Why | Tracked in |
|---|---|---|
| Per-workspace restore fidelity | needs compositor APIs | MIGRATION.md |
| `session.greeter_session_id` auto-discovery | needs greeter↔session handshake | MIGRATION.md |
| lion-auth real client against spec 22 | spec 22 not built | MIGRATION.md |
| X-GNOME-Autostart-Delay < 1s granularity | upstream convention is seconds | — |
