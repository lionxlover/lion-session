//! Lightweight session metrics: lock-free counters snapshotted as JSON for
//! the `GetMetrics()` D-Bus method (same shape as lion-greeter's, so
//! LionOS tooling has one consistent metrics dialect).
//!
//! Zero dependencies, no allocation outside `snapshot_json()`, and every
//! counter is `Relaxed` ordering: these are telemetry, not synchronization.

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

/// Every counter the session tracks. Order is part of the wire format:
/// do not reorder, only append.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Counter {
    /// Compositor died and was respawned by the crash-recovery loop.
    CompositorRestarts = 0,
    /// Compositor crashed too often and ended the session.
    CompositorCrashloop = 1,
    /// Autostart children started (first start only).
    AppsStarted = 2,
    /// Autostart children restarted after a failure.
    AppRestarts = 3,
    /// Autostart children that hit the crash-loop guard.
    AppCrashloops = 4,
    /// D-Bus clients registered for the end-of-session protocol.
    ClientRegistrations = 5,
    /// Session-level inhibitors currently/taken in total.
    SessionInhibitors = 6,
    /// logind delay inhibitors held (one per end sequence that got one).
    LogindInhibitorHolds = 7,
    /// Lock requests forwarded to lion-locker.
    LockRequests = 8,
    /// Idle hints pushed to logind.
    IdleHints = 9,
    /// End sequences that asked apps to save state.
    QueryEnds = 10,
    /// End sequences cancelled because an app vetoed a logout.
    VetoedEnds = 11,
    /// End sequences forced after the answer timeout.
    ForcedEnds = 12,
    /// 0.3.0: idle escalations that locked the screen.
    IdleLocks = 13,
    /// 0.3.0: idle escalations that ended the session (kiosk logout).
    IdleLogouts = 14,
    /// 0.3.0: locks issued because logind announced an imminent
    /// shutdown (lock-on-shutdown).
    ShutdownLocks = 15,
}

pub const COUNTER_NAMES: [&str; 16] = [
    "compositor_restarts",
    "compositor_crashloops",
    "apps_started",
    "app_restarts",
    "app_crashloops",
    "client_registrations",
    "session_inhibitors",
    "logind_inhibitor_holds",
    "lock_requests",
    "idle_hints",
    "query_ends",
    "vetoed_ends",
    "forced_ends",
    "idle_locks",
    "idle_logouts",
    "shutdown_locks",
];

#[derive(Debug)]
pub struct Metrics {
    started_ms: u64,
    counters: [AtomicU64; COUNTER_NAMES.len()],
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            started_ms: unix_ms(),
            counters: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    pub fn inc(&self, c: Counter) {
        self.counters[c as usize].fetch_add(1, Ordering::Relaxed);
    }

    /// Get the value of one counter (test/telemetry seam).
    #[cfg(test)]
    pub fn get(&self, c: Counter) -> u64 {
        self.counters[c as usize].load(Ordering::Relaxed)
    }

    /// Hand-built JSON -- no serde_json dependency for one flat object.
    pub fn snapshot_json(&self) -> String {
        let uptime = unix_ms().saturating_sub(self.started_ms);
        let mut s = format!(
            "{{\"started_ms\":{},\"uptime_ms\":{}",
            self.started_ms, uptime
        );
        for (i, name) in COUNTER_NAMES.iter().enumerate() {
            s.push_str(&format!(
                ",\"{name}\":{}",
                self.counters[i].load(Ordering::Relaxed)
            ));
        }
        s.push('}');
        s
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_lists_every_counter() {
        let m = Metrics::new();
        let json = m.snapshot_json();
        for name in COUNTER_NAMES {
            assert!(
                json.contains(&format!("\"{name}\":")),
                "missing {name} in {json}"
            );
        }
        assert!(json.starts_with('{') && json.ends_with('}'));
        assert!(json.contains("\"uptime_ms\":"));
    }

    #[test]
    fn counters_are_independent() {
        let m = Metrics::new();
        m.inc(Counter::LockRequests);
        m.inc(Counter::LockRequests);
        m.inc(Counter::CompositorRestarts);
        assert_eq!(m.get(Counter::LockRequests), 2);
        assert_eq!(m.get(Counter::CompositorRestarts), 1);
        assert_eq!(m.get(Counter::IdleHints), 0);
    }

    #[test]
    fn counter_names_align_with_enum_order() {
        assert_eq!(COUNTER_NAMES[Counter::VetoedEnds as usize], "vetoed_ends");
        assert_eq!(COUNTER_NAMES[Counter::ForcedEnds as usize], "forced_ends");
        assert_eq!(COUNTER_NAMES[Counter::IdleLocks as usize], "idle_locks");
        assert_eq!(COUNTER_NAMES[Counter::IdleLogouts as usize], "idle_logouts");
        assert_eq!(
            COUNTER_NAMES[Counter::ShutdownLocks as usize],
            "shutdown_locks"
        );
        assert_eq!(COUNTER_NAMES.len(), 16);
    }
}
