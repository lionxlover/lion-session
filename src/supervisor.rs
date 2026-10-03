#![forbid(unsafe_code)]
//! Supervision decisions: restart policy, exponential backoff and
//! crash-loop detection (spec 02 §3/§6).
//!
//! This module is pure bookkeeping: every function takes an explicit
//! `now: Instant`, so tests exercise minutes of windowing without
//! sleeping. The orchestration layer in `session.rs` turns [`Decision`]s
//! into real delays and spawns.
//!
//! Backoff: delay doubles from `backoff_start_ms` up to `backoff_max_ms`,
//! computed from restart attempts inside the crash window — a service
//! flapping at 50 ms cadence backs off quickly instead of burning CPU.
//! Give-up: `max_failures` *crashes* (non-zero exits) inside `window_ms`
//! stop the retry loop and surface a single coalesced notification.

use crate::config::{CrashLoopConfig, RestartPolicy};
use std::time::{Duration, Instant};

/// Bookkeeping for one supervised service.
#[derive(Debug, Clone, Default)]
pub struct SupState {
    /// Timestamps of crashes (non-zero exits) inside the window.
    failures: Vec<Instant>,
    /// Timestamps of restart attempts inside the window (drives backoff).
    restarts: Vec<Instant>,
    /// Last instant a ServiceFailed notification was emitted for this
    /// service (coalescing: one notification, not a storm).
    last_notified: Option<Instant>,
    /// Set when the retry loop gave up; only a fresh successful run clears.
    given_up: bool,
}

impl SupState {
    /// Prune both windows to entries newer than `window` before `now`.
    fn prune(&mut self, now: Instant, window: Duration) {
        let cutoff = now.checked_sub(window).unwrap_or(now);
        self.failures.retain(|t| *t >= cutoff);
        self.restarts.retain(|t| *t >= cutoff);
    }

    /// Crashes counted inside the current window.
    pub fn failures_in_window(&self) -> usize {
        self.failures.len()
    }

    /// True once the retry loop gave up for this service.
    pub fn is_given_up(&self) -> bool {
        self.given_up
    }

    /// Next restart delay, given the attempts already inside the window.
    fn backoff(&self, cl: &CrashLoopConfig) -> Duration {
        let attempts = self.restarts.len() as u32;
        let start = cl.backoff_start_ms.max(1);
        let shift = attempts.min(16);
        let base = start.saturating_mul(1u64 << shift).max(start);
        Duration::from_millis(base.min(cl.backoff_max_ms.max(start)))
    }
}

/// What the supervisor should do after a service exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Restart after this delay (backoff included).
    RestartAfter(Duration),
    /// The service stays down (policy Never, clean exit, or crash-loop).
    StayDown,
    /// Crash-loop tripped: stop retrying AND notify the user (coalesced).
    GiveUpAndNotify,
}

/// Record a service exit (abnormal flag + exit code) and decide.
///
/// `exit_ok` = exit code 0. `crash` = abnormal termination or non-zero.
/// For the compositor the caller ignores the decision and ends the
/// session instead (spec 02 §3) — supervision policies apply to shell
/// services.
pub fn on_exit(
    state: &mut SupState,
    policy: RestartPolicy,
    cl: &CrashLoopConfig,
    now: Instant,
    exit_ok: bool,
) -> Decision {
    state.prune(now, Duration::from_millis(cl.window_ms));

    let crash = !exit_ok;
    if crash {
        state.failures.push(now);
    }

    // Already gave up: stays down (one notification, not a storm —
    // repeated GiveUpAndNotify would re-notify on every exit).
    if state.given_up {
        return Decision::StayDown;
    }

    // Crash-loop: N failures within the window → stop retrying.
    if state.failures.len() >= cl.max_failures as usize {
        state.given_up = true;
        return Decision::GiveUpAndNotify;
    }

    match policy {
        RestartPolicy::Never => Decision::StayDown,
        RestartPolicy::OnFailure if exit_ok => Decision::StayDown,
        RestartPolicy::OnFailure | RestartPolicy::Always => {
            let delay = state.backoff(cl);
            state.restarts.push(now);
            Decision::RestartAfter(delay)
        }
    }
}

/// A service ran healthy for a while: clear the given-up flag and the
/// windows (the flapping episode is over).
pub fn on_healthy(state: &mut SupState, now: Instant, cl: &CrashLoopConfig) {
    state.prune(now, Duration::from_millis(cl.window_ms));
    if state.given_up {
        state.given_up = false;
        state.failures.clear();
        state.restarts.clear();
    }
}

/// May we emit a ServiceFailed notification for this service now
/// (coalescing, spec 02 §6: back off and show one notification)?
pub fn should_notify(state: &mut SupState, cl: &CrashLoopConfig, now: Instant) -> bool {
    let gap = Duration::from_millis(cl.notify_coalesce_ms);
    let allowed = state
        .last_notified
        .map(|t| now.duration_since(t) >= gap)
        .unwrap_or(true);
    if allowed {
        state.last_notified = Some(now);
    }
    allowed
}

/// Reason string for the ServiceFailed signal (deliberately generic —
/// no argv, no environment, no paths of the failing binary).
pub fn failure_reason(crashes: usize, window_ms: u64) -> String {
    format!("crashed {crashes} times within {window_ms} ms; restarts suspended")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cl() -> CrashLoopConfig {
        CrashLoopConfig {
            max_failures: 3,
            window_ms: 10_000,
            backoff_start_ms: 100,
            backoff_max_ms: 5000,
            notify_coalesce_ms: 5000,
        }
    }

    thread_local! {
        static BASE: Instant = Instant::now();
    }

    /// All t() values share one base: real Instant::now() drifts between
    /// calls, which would prune window edges spuriously.
    fn t(ms: u64) -> Instant {
        BASE.with(|b| *b + Duration::from_millis(ms))
    }

    #[test]
    fn never_policy_stays_down() {
        let mut s = SupState::default();
        let d = on_exit(&mut s, RestartPolicy::Never, &cl(), t(0), false);
        assert_eq!(d, Decision::StayDown);
    }

    #[test]
    fn on_failure_clean_exit_stays_down() {
        let mut s = SupState::default();
        let d = on_exit(&mut s, RestartPolicy::OnFailure, &cl(), t(0), true);
        assert_eq!(d, Decision::StayDown);
        // and it was not counted as a crash
        assert_eq!(s.failures_in_window(), 0);
    }

    #[test]
    fn on_failure_crash_restarts_with_backoff() {
        let mut s = SupState::default();
        let d = on_exit(&mut s, RestartPolicy::OnFailure, &cl(), t(0), false);
        assert_eq!(d, Decision::RestartAfter(Duration::from_millis(100)));

        let d = on_exit(&mut s, RestartPolicy::OnFailure, &cl(), t(50), false);
        assert_eq!(d, Decision::RestartAfter(Duration::from_millis(200)));

        let d = on_exit(&mut s, RestartPolicy::OnFailure, &cl(), t(100), false);
        // third crash inside window trips the loop (max_failures = 3)
        assert_eq!(d, Decision::GiveUpAndNotify);
        assert!(s.is_given_up());
    }

    #[test]
    fn always_restarts_even_on_clean_exit() {
        let mut s = SupState::default();
        let d = on_exit(&mut s, RestartPolicy::Always, &cl(), t(0), true);
        assert!(matches!(d, Decision::RestartAfter(_)));
        assert_eq!(s.failures_in_window(), 0, "clean exit is not a crash");
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let cl = CrashLoopConfig {
            max_failures: 100,
            window_ms: 60_000,
            backoff_start_ms: 100,
            backoff_max_ms: 5000,
            notify_coalesce_ms: 5000,
        };
        let cl_ref = &cl;
        let mut s = SupState::default();
        let mut now = 0u64;
        // Clean exits under Always: restarts (not crashes) are counted, so
        // the give-up path never trips; the wide window keeps them all.
        for expect_ms in [100u64, 200, 400, 800, 1600, 3200, 5000, 5000] {
            let d = on_exit(&mut s, RestartPolicy::Always, cl_ref, t(now), true);
            match d {
                Decision::RestartAfter(d) => {
                    assert_eq!(d.as_millis() as u64, expect_ms, "at t={now}");
                }
                other => panic!("expected restart, got {other:?} at t={now}"),
            }
            now += 2000;
        }
        let _ = cl_ref;
    }

    #[test]
    fn old_failures_expire_from_window() {
        let mut s = SupState::default();
        on_exit(&mut s, RestartPolicy::OnFailure, &cl(), t(0), false);
        on_exit(&mut s, RestartPolicy::OnFailure, &cl(), t(100), false);
        assert_eq!(s.failures_in_window(), 2);
        // 20 s later the window has drained.
        on_exit(&mut s, RestartPolicy::OnFailure, &cl(), t(20_000), false);
        assert_eq!(s.failures_in_window(), 1, "old entries pruned");
    }

    #[test]
    fn given_up_stays_down_until_healthy() {
        let mut s = SupState::default();
        on_exit(&mut s, RestartPolicy::OnFailure, &cl(), t(0), false);
        on_exit(&mut s, RestartPolicy::OnFailure, &cl(), t(10), false);
        let d = on_exit(&mut s, RestartPolicy::OnFailure, &cl(), t(20), false);
        assert_eq!(d, Decision::GiveUpAndNotify);
        // further exits stay down
        let d = on_exit(&mut s, RestartPolicy::OnFailure, &cl(), t(30), false);
        assert_eq!(d, Decision::StayDown);
        // healthy run resets
        on_healthy(&mut s, t(10_000), &cl());
        assert!(!s.is_given_up());
        let d = on_exit(&mut s, RestartPolicy::OnFailure, &cl(), t(10_050), false);
        assert!(matches!(d, Decision::RestartAfter(_)));
    }

    #[test]
    fn notifications_coalesce() {
        let mut s = SupState::default();
        assert!(should_notify(&mut s, &cl(), t(0)));
        assert!(!should_notify(&mut s, &cl(), t(1000)), "inside gap");
        assert!(!should_notify(&mut s, &cl(), t(4999)), "still inside");
        assert!(should_notify(&mut s, &cl(), t(5001)), "gap elapsed");
    }

    #[test]
    fn failure_reason_is_generic() {
        let r = failure_reason(5, 15000);
        assert!(r.contains("crashed 5 times"));
        assert!(!r.contains("/"), "no paths leaked");
    }
}
