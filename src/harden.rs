//! Process-level hardening applied to the session manager itself.
//!
//! The 0.2.0 report's honesty section said the vendor stacks score
//! higher on privilege hygiene because their credential surfaces are
//! kernel-adjacent. The session-manager-side of that gap closes here
//! with three best-effort, self-applied measures:
//!
//! * **`PR_SET_DUMPABLE = 0`** — the manager becomes non-dumpable:
//!   same-user processes can no longer `ptrace` it, read
//!   `/proc/<pid>/mem` or `/proc/<pid>/environ` of it, or core-dump
//!   it. Within a desktop session the realistic attacker IS a
//!   same-user process (a compromised app); this is the cheapest
//!   anti-snooping boundary available without a sandbox.
//!   (Dumpability is per-process and recomputed on `execve`, so
//!   children are unaffected — apps keep their normal dumpability
//!   and `sudo`-style privilege bits keep working.)
//! * **`RLIMIT_CORE = 0`** — a crashing manager never leaves a core
//!   file carrying session state on disk.
//! * **`PR_SET_NO_NEW_PRIVS` is deliberately NOT set**: it is
//!   inherited by every child, and a desktop session manager is the
//!   *parent of user applications* — setting it would break `sudo`,
//!   `pkexec`, setuid helpers and anything else that legitimately
//!   elevates from within the session. Hardening that breaks the
//!   desktop is not hardening.
//!
//! Everything here is best-effort: a container that refuses a prctl
//! gets a warning line and a session that still works (the fail-open
//! rule that governs every integration in this daemon).

use std::time::{SystemTime, UNIX_EPOCH};

/// Outcome of [`apply`] — reported in `--version`, the journal, and
/// the metrics snapshot, so "hardened" is a verifiable state, not a
/// design intention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HardenReport {
    pub dumpable_off: bool,
    pub core_dumps_off: bool,
}

impl HardenReport {
    pub fn tag(&self) -> &'static str {
        match (self.dumpable_off, self.core_dumps_off) {
            (true, true) => "full",
            (true, false) => "dumpable-only",
            (false, true) => "core-only",
            (false, false) => "none",
        }
    }
}

/// Apply all measures; log and continue on any refusal.
pub fn apply() -> HardenReport {
    let dumpable_off = set_dumpable(true);
    if !dumpable_off {
        tracing::warn!("PR_SET_DUMPABLE refused; session manager stays dumpable");
    }
    let core_dumps_off = set_core_limit_zero();
    if !core_dumps_off {
        tracing::warn!("RLIMIT_CORE=0 refused; core dumps remain possible");
    }
    let report = HardenReport {
        dumpable_off,
        core_dumps_off,
    };
    tracing::info!(level = report.tag(), "process hardening applied");
    report
}

/// `PR_SET_DUMPABLE(0)` — see module docs.
fn set_dumpable(off: bool) -> bool {
    // SAFETY: prctl with an integer option and a 0/1 argument; the
    // return value is checked, and the call cannot fault.
    unsafe { libc::prctl(libc::PR_SET_DUMPABLE, if off { 0 } else { 1 }) == 0 }
}

/// `RLIMIT_CORE = {0, 0}` — see module docs.
fn set_core_limit_zero() -> bool {
    let rl = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: setrlimit with a stack-local, properly shaped struct.
    unsafe { libc::setrlimit(libc::RLIMIT_CORE, &rl) == 0 }
}

/// Read back the current dumpability (verification seam).
#[cfg(test)]
pub fn dumpable_now() -> i32 {
    // SAFETY: prctl read-only query.
    unsafe { libc::prctl(libc::PR_GET_DUMPABLE) }
}

/// Deterministic per-process jitter factor in `[0, max_add_ms]`.
///
/// Used to de-synchronize restart backoffs: when a display hiccup
/// kills five apps in the same millisecond, their restarts without
/// jitter would also align — a thundering herd on every subsequent
/// restart. Seeded from the pid + clock so it varies per process and
/// per call site but stays reproducible in logs.
pub fn jitter_ms(max_add_ms: u64) -> u64 {
    if max_add_ms == 0 {
        return 0;
    }
    let pid = std::process::id() as u64;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    // xorshift64* over (pid, clock).
    let mut x = pid ^ now ^ 0x9E37_79B9_7F4A_7C15;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    let mixed = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
    mixed % (max_add_ms + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test for all process-global mutations: dumpability is
    /// process state, so two tests flipping it would interleave under
    /// the default parallel runner.
    #[test]
    fn hardening_applies_and_round_trips_in_this_process() {
        let report = apply();
        assert_eq!(report.dumpable_off, dumpable_now() == 0);
        // Read RLIMIT_CORE back through getrlimit.
        // SAFETY: getrlimit into a zeroed struct.
        let mut rl = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        let rc = unsafe { libc::getrlimit(libc::RLIMIT_CORE, &mut rl) };
        if rc == 0 && report.core_dumps_off {
            assert_eq!(rl.rlim_cur, 0);
        }
        // Tag is consistent with the fields.
        let tag = report.tag();
        assert_eq!(tag == "full", report.dumpable_off && report.core_dumps_off);
        // Dumpability round-trips; leave the process non-dumpable at
        // the end (apply() semantics restored).
        assert!(set_dumpable(false));
        assert_eq!(dumpable_now(), 1);
        assert!(set_dumpable(true));
        assert_eq!(dumpable_now(), 0);
    }

    #[test]
    fn jitter_stays_in_bounds_and_varies() {
        for _ in 0..200 {
            let j = jitter_ms(100);
            assert!(j <= 100);
        }
        // Different seeds give different sequences (not a proof of
        // uniformity, a regression guard against a stuck constant).
        let a: Vec<u64> = (0..20).map(|_| jitter_ms(1000)).collect();
        assert!(a.iter().any(|v| *v != a[0]));
        assert_eq!(jitter_ms(0), 0);
    }
}
