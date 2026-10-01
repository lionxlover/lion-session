# Security policy — lion-session

## Reporting

Report vulnerabilities to **security@lionos.org** (PGP key at
https://lionos.org/.well-known/security.txt). Include reproduction
steps and `lion-session --version` output. Aim: acknowledgement
< 48 h, assessment < 7 days, coordinated disclosure with a patch
release, reporter credit unless anonymity is requested.

Do not open public issues for suspected vulnerabilities.

## Threat model (summary)

The manager runs as the logged-in user, is the parent of every
autostart app, and drives logind power actions.

| Surface | Mitigation |
|---|---|
| Same-user process snooping (compromised app) | `PR_SET_DUMPABLE=0`: no ptrace, no `/proc/<pid>/mem` or `environ` reads of the manager |
| Crash artifacts | `RLIMIT_CORE=0` on self and children via teardown signals |
| Malicious autostart/desktop files | Hand-audited Exec tokenizer (NUL-excising, quoting rules), `--property` value validation, TryExec/OnlyShowIn lister rules, deterministic fuzz harness in CI |
| Runaway children | Per-app cgroup limits (MemoryMax/CPUWeight/TasksMax), bounded end protocol, PDEATHSIG orphan cleanup, kill_on_drop |
| Denial of the logout path | Bounded end-timeout, 2 s per-task teardown cap, delay inhibitor released by fd-drop |
| Supply chain | 10 dependencies, `--locked` builds, cargo-audit in CI, fuzz harness every push |
| System power abuse | Power forwards go through lion-power's own policy; the manager never calls logind PowerOff directly |

## Deliberate non-hardening (documented trade-offs)

* `PR_SET_NO_NEW_PRIVS` is NOT set: it is inherited by every child and
  would break `sudo`/`pkexec`/setuid helpers for user applications.
  The secure-attention surface is `lion-locker`'s (#3) design.
* The manager stays ptrace-able by root (root is out of scope for
  same-user threat models; dumpable=0 only stops non-root).

## Fuzzing & audit status

* Deterministic fuzz harness: every CI run (TOML config layering,
  Exec tokenizer, merge-by-name invariant).
* Formal third-party audit: LionOS 1.0 milestone; the dependency
  count and parser surface are deliberately small to keep it deep.
