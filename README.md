# lion-session

**LionOS per-user session supervisor** (spec 02) — starts and supervises
the compositor and every shell service, owns the session lifecycle, and
executes logout/restart/shutdown/suspend/hibernate/lock/switch-user
through logind, with inhibitors respected.

- Bus service: `os.lionos.Session1` on the session bus
  (`/os/lionos/Session1`) — see `docs/DBUS.md`.
- Written in Rust; `#![forbid(unsafe_code)]` everywhere except the single
  audited FFI module (`sysffi`: pidfd).
- Event-driven: one core task, zero polling, no timers when idle.

## Quick start

```sh
cargo build --release
./target/release/lion-session --version
./target/release/lion-session --print-schema
./target/release/lion-session --check-config --config conf/lion-session.json
# demo (no D-Bus, script fakes): full startup → SessionReady → autostart
./target/release/lion-session --mock --config conf/lion-session.json
```

Reference client:

```sh
cargo build --example session_client
./target/release/examples/session_client status
```

## Layout

| Path | What |
|---|---|
| `src/session.rs` | The core: startup orchestration, supervision, lifecycle, event loop |
| `src/bus.rs` | The `os.lionos.Session1` D-Bus service (zbus 5) |
| `src/resolver.rs` | Dependency-order resolver (property-tested) |
| `src/supervisor.rs` | Restart policy, backoff, crash-loop detection |
| `src/inhibitors.rs` | fd-owned inhibitors + rate bounds |
| `src/lifecycle.rs` | The end-session state machine (pure) |
| `src/autostart.rs` | XDG autostart `.desktop` parsing (fuzzed) |
| `src/environment.rs` | Session environment construction |
| `src/safemode.rs` / `src/savestate.rs` | Safe mode & session restore |
| `src/backends.rs` | Direct launcher; zbus systemd/logind + CLI fallbacks |
| `src/mocks.rs` | Fakes for every port (tests + `--mock`) |
| `tests/dbus_e2e.rs` | Real-bus acceptance: private dbus-daemon, fake logind, real binary |

## Configuration

`/etc/lion/session.json` (lion-config key file; schema:
`packaging/lion-config/session.schema.json`, printable with
`--print-schema`). Spec keys:

```json
{
  "session": {
    "shutdown_timeout_ms": 8000,
    "restore_apps": false,
    "autostart_delay_ms": 1500
  }
}
```

…plus documented extensions for services, supervision, crash-loop
policy, inhibit limits, authorization wiring, safe mode and startup
timeouts (see the schema and `DESIGN.md`).

## Safety & security posture

- Callers are identified by the bus daemon (uid/pid), pid-pinned via
  pidfd, cgroup recorded; never by caller-supplied strings.
- Authorization through lion-auth (`os.lionos.Auth1`) — **fail closed**
  on any error/timeout; a local-policy mode exists for development.
- Every input validated and bounded; expensive calls rate-limited.
- Secrets never exist in this component; logs are structured
  (`tracing`) with no environment/argv dumps in errors.

## Verification

```sh
cargo test                       # 142 unit/property tests
cargo test --test dbus_e2e      # real-bus acceptance (needs dbus-daemon)
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo bench --bench lion_bench
```

CI (`.github/workflows/ci.yml`) runs the same gates plus deny, a fuzz
smoke and a systemd-nspawn acceptance script
(`tests/ci-nspawn-acceptance.sh`).

## Milestones (spec 02 §11)

- **MVP** — environment setup, compositor + shell launch,
  logout/shutdown: **done, tested** (unit + property + real-bus e2e).
- **v1** — supervision, crash-loop detection, inhibitors, autostart:
  **done, tested** (crash/hang/flap fakes in `src/session.rs` tests,
  inhibitor fd semantics in `tests/dbus_e2e.rs`).
- **v2** — session restore, safe mode, startup profiling hooks:
  **implemented** (model + persistence + safe-mode boot decision +
  `startup_us` metric); relaunch of restored apps runs through the
  ordinary autostart path; further workspace-fidelity restore is
  deferred (see `DESIGN.md`).

See `DESIGN.md` for decisions and trade-offs, `MIGRATION.md` for
config/interface evolution.
