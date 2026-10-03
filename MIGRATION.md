# lion-session — MIGRATION.md

Interface, configuration and behavior evolution. Semver-style: while the
project is 0.x, breaking changes are allowed but must land here.

## Config

| Version | Change | Migration |
|---|---|---|
| 0.1.0 | Initial schema (`session.*` tree, `packaging/lion-config/session.schema.json`). | — |
| planned 0.2 | `session.lion_auth.bus_name` switches from `""` (local policy) to `os.lionos.Auth1` once lion-auth (spec 22) ships. | Set the bus name in `/etc/lion/session.json` (the default already does this); ensure lion-auth is reachable or privileged calls will deny (fail closed). |
| planned 0.2 | `session.greeter_session_id` may be auto-discovered from the greeter handshake. | Remove the manual id; keep it to pin a specific greeter session. |
| planned 0.3 | Workspace-fidelity session restore (per-window workspace ids from the compositor). | No action; `restore_apps` stays opt-in. |

`--check-config` and `--print-schema` are the compatibility tools:
unknown fields are rejected (fail closed), so a config written for a
newer lion-session fails loudly on older versions instead of silently
ignoring keys.

## D-Bus interface (`os.lionos.Session1`)

- 0.1.0: spec 02 §4 surface complete. Additive, documented extensions:
  `EndSessionReply(app_id)` method, `Blockers` property
  (`docs/DBUS.md`).
- Clients should treat **unknown methods/properties as absent** (probe
  by name, not by version) — no interface version property is exposed
  on purpose (additive-only policy).
- `InhibitedActions` is a sorted, de-duplicated list; do not rely on
  ordering beyond that.

## State files

| File | Introduced | Notes |
|---|---|---|
| `$XDG_STATE_HOME/lion-session/history.json` | 0.1.0 | Safe-mode bookkeeping; corrupt files reset with a warning. |
| `$XDG_STATE_HOME/lion-session/state.json` | 0.1.0 | Session-restore snapshot (opt-in); corrupt files are ignored. |

Both use atomic-rename writes; readers treat missing files as first
boot.

## Behavior notes

- `State` property values: `starting`, `running`, `query-end-session`,
  `ending`, `ended` (0.1.0). `ended` is transient before process exit —
  clients should treat bus-name loss as session end too.
- Flags on query/end signals: `0x1` force, `0x2` restart (documented in
  `docs/DBUS.md`).
- Suppression of XDG autostart in safe mode is intentional and will
  stay.
