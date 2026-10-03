# `os.lionos.Session1` — D-Bus interface (spec 02 §4)

The per-user session supervisor serves this interface on the **session
bus** at object path `/os/lionos/Session1`.

The canonical machine-readable definition ships in
`packaging/data/os.lion.Session1.xml` (introspection XML).

## Methods

| Method | Signature | Meaning |
|---|---|---|
| `Logout` | `() → ()` | Begin the end-session flow (query apps, respect inhibitors, force after `session.shutdown_timeout_ms`). |
| `Restart` | `() → ()` | Same flow, then `logind.Reboot()`. |
| `Shutdown` | `() → ()` | Same flow, then `logind.PowerOff()`. |
| `Suspend` | `() → ()` | `logind.Suspend()` directly (inhibitors of the `suspend` class are checked first). |
| `Hibernate` | `() → ()` | `logind.Hibernate()` (inhibitors as above). |
| `Lock` | `() → ()` | `logind.LockSession($XDG_SESSION_ID)` / `LockSessions()`. |
| `SwitchUser` | `() → ()` | `LockSessions()`, then `ActivateSession(session.greeter_session_id)` when configured. |
| `Inhibit` | `(what: s, who: s, why: s) → h` | Take an inhibitor; the returned fd *owns* it (closing releases). |
| `RegisterClient` | `(app_id: s) → ()` | Register for end-session queries (bus disconnect also counts as an ack). |

### Documented LionOS extensions

| Method | Signature | Meaning |
|---|---|---|
| `EndSessionReply` | `(app_id: s) → ()` | Acknowledge `QueryEndSession` without disconnecting. |

## Signals

| Signal | Signature | Meaning |
|---|---|---|
| `SessionReady` | `()` | Startup complete: compositor up, ready-gate services up. Boot metrics anchor. |
| `QueryEndSession` | `(flags: u)` | Apps should prepare to close. |
| `EndSession` | `(flags: u)` | Apps must close; force follows. |
| `ServiceFailed` | `(name: s, reason: s)` | A supervised service gave up after crash-looping (coalesced). |

### Flags

- `0x1` (`FLAG_FORCE`) — the timeout expired; the end was forced.
- `0x2` (`FLAG_RESTART`) — the session will restart immediately after.

## Properties

| Property | Type | Meaning |
|---|---|---|
| `State` | `s` | `starting` \| `running` \| `query-end-session` \| `ending` \| `ended` |
| `InhibitedActions` | `as` | Union of active inhibitor classes. |
| `SafeMode` | `b` | This boot is the minimal shell. |

### Extension properties

| Property | Type | Meaning |
|---|---|---|
| `Blockers` | `as` | Non-acked clients + `inhibitor: WHO (WHY)` entries during an end-session query ("show which app is blocking", spec §3). |

All properties emit `org.freedesktop.DBus.Properties.PropertiesChanged`.

## Inhibitor semantics (spec 02 §6)

- `what` ∈ `logout`, `shutdown`, `suspend`, `idle`, `switch-user`
  (anything else → `InvalidArgs`).
- `who`/`why` are bounded (256/512 bytes) and recorded in `Blockers`.
- The returned fd is a socketpair end: dropping it (or the client dying)
  releases the inhibitor automatically — an inhibitor leak self-heals.
- Inhibitor classes block the matching lifecycle actions; a `shutdown`
  inhibitor blocks `Restart` and `Shutdown`, not `Logout`.

## Caller identity (spec 02 §8)

- Callers are identified by the **bus daemon** (`GetConnectionUnixUser`,
  `GetConnectionUnixProcessID`) — never by caller-supplied strings.
- The pid is pinned with a pidfd and its cgroup recorded in audit logs.
- Authorization: `lion-auth` (`os.lionos.Auth1.Authorize(action, uid)`)
  when configured; fail-closed on any transport error or timeout. With
  `session.lion_auth.bus_name = ""` a local policy applies (session
  owner, allowlists, root — see DESIGN.md).
- Rate limits: `Inhibit` and `RegisterClient` are bounded per caller per
  minute; excess is denied.

## Reference client

`examples/session_client.rs` (build: `cargo build --example
session_client`) exercises every method, property and signal:

```
session_client status
session_client register lion-text        # registers, acks queries
session_client inhibit shutdown lion-text "compile job"
session_client logout
session_client signals                   # watch all signals
```
