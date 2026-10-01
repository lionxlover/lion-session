//! Idle escalation: what happens *after* the session goes idle.
//!
//! 0.2.0 pushed the idle hint to logind and called it a day — the
//! same policy depth as xfce4-session. GNOME delegates escalation
//! (idle -> dim -> lock -> logout) to gnome-settings-daemon; macOS
//! and Windows own it in powerd / power policy. 0.3.0 gives the
//! session manager itself the two escalations that actually protect
//! data, without a settings daemon in the loop:
//!
//! * **`lock-after-ms`**: once the session reports idle, lock it
//!   after a grace period. Resume-from-idle then requires
//!   re-authentication — the anti-shoulder-surf default GNOME and
//!   macOS both ship.
//! * **`logout-after-ms`**: after (more) idle, end the session —
//!   the kiosk / shared-lab policy (auto-return to the greeter).
//!
//! # Design
//! A pure state machine (`IdleMachine`) driven by two inputs:
//! `set_idle(bool)` reports (from `SetIdle` on the D-Bus surface, i.e.
//! whatever idle detector the desktop runs — lion-idle or anything
//! else) and `poll()` (time passing, 1 s tick). The machine decides
//! *when* to escalate; the caller decides *how* (forward Lock to
//! lion-locker; end the session through the normal cooperative
//! protocol). This split keeps the policy unit-testable without a
//! bus, a locker, or real time.
//!
//! # Honesty
//! When nothing is configured (the default), behaviour is
//! bit-for-bit 0.2.0: the hint is forwarded, nothing escalates.

use serde::Deserialize;
use std::time::{Duration, Instant};

/// Idle policy from `[idle]` in session.toml. Zero = disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct IdlePolicy {
    pub lock_after_ms: u64,
    pub logout_after_ms: u64,
}

impl IdlePolicy {
    #[allow(dead_code)] // used by tests; kept for symmetry with Config
    pub fn disabled() -> Self {
        Self::default()
    }

    pub fn is_configured(&self) -> bool {
        self.lock_after_ms > 0 || self.logout_after_ms > 0
    }

    fn lock_after(&self) -> Option<Duration> {
        (self.lock_after_ms > 0).then_some(Duration::from_millis(self.lock_after_ms))
    }

    fn logout_after(&self) -> Option<Duration> {
        (self.logout_after_ms > 0).then_some(Duration::from_millis(self.logout_after_ms))
    }
}

/// One escalation decision. Ordered: a poll returns at most the
/// strongest new action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleAction {
    /// Nothing to do.
    None,
    /// Idle duration crossed `lock-after-ms`: lock now.
    Lock,
    /// Idle duration crossed `logout-after-ms`: end the session now.
    EndSession,
}

/// Pure idle-escalation state machine.
#[derive(Debug)]
pub struct IdleMachine {
    policy: IdlePolicy,
    idle_since: Option<Instant>,
    locked: bool,
    ended: bool,
}

impl IdleMachine {
    pub fn new(policy: IdlePolicy) -> Self {
        Self {
            policy,
            idle_since: None,
            locked: false,
            ended: false,
        }
    }

    /// Report the session's idle state: entering idle starts the
    /// timers, leaving it clears them. The *decision* to escalate is
    /// made exclusively by [`poll`] — one decision point, driven by
    /// the 1 Hz tick, keeps the semantics total: any configured
    /// timeout fires within one tick of being reached.
    pub fn set_idle(&mut self, idle: bool) {
        if idle {
            if self.idle_since.is_none() {
                self.idle_since = Some(Instant::now());
                self.locked = false;
            }
        } else {
            self.idle_since = None;
            self.locked = false;
        }
    }

    /// Time passes. Call ~1x/s. Returns the strongest new action.
    pub fn poll(&mut self) -> IdleAction {
        let Some(since) = self.idle_since else {
            return IdleAction::None;
        };
        let elapsed = since.elapsed();
        if self.ended {
            return IdleAction::None;
        }
        if let Some(after) = self.policy.logout_after() {
            if elapsed >= after {
                self.ended = true;
                return IdleAction::EndSession;
            }
        }
        if !self.locked {
            if let Some(after) = self.policy.lock_after() {
                if elapsed >= after {
                    self.locked = true;
                    return IdleAction::Lock;
                }
            }
        }
        IdleAction::None
    }

    /// Mark the machine as having locked (used by the caller when a
    /// Lock request arrives from elsewhere, e.g. loginctl), so poll()
    /// does not double-lock. (Reserved for the loginctl-lock bridge;
    /// tested now, wired when lion-locker lands.)
    #[allow(dead_code)]
    pub fn note_locked(&mut self) {
        self.locked = true;
    }

    /// True while the idle timer is running.
    #[allow(dead_code)]
    pub fn is_idle(&self) -> bool {
        self.idle_since.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(lock: u64, logout: u64) -> IdlePolicy {
        IdlePolicy {
            lock_after_ms: lock,
            logout_after_ms: logout,
        }
    }

    #[test]
    fn disabled_policy_never_escalates() {
        let mut m = IdleMachine::new(IdlePolicy::disabled());
        m.set_idle(true);
        for _ in 0..100 {
            assert_eq!(m.poll(), IdleAction::None);
        }
        assert!(!IdlePolicy::disabled().is_configured());
    }

    #[test]
    fn tiny_lock_timeout_locks_on_first_poll() {
        let mut m = IdleMachine::new(policy(1, 0));
        m.set_idle(true);
        std::thread::sleep(Duration::from_millis(3));
        assert_eq!(m.poll(), IdleAction::Lock);
    }

    #[test]
    fn tiny_logout_timeout_ends_on_first_poll() {
        let mut m = IdleMachine::new(policy(500, 1));
        m.set_idle(true);
        std::thread::sleep(Duration::from_millis(3));
        assert_eq!(m.poll(), IdleAction::EndSession);
    }

    #[test]
    fn lock_then_logout_sequence() {
        // Simulated clock: we can't fake Instant, so use 1ms timeouts
        // and real sleeps bounded tightly.
        let mut m = IdleMachine::new(policy(1, 30));
        m.set_idle(true);
        // First poll after >=1ms -> Lock.
        std::thread::sleep(Duration::from_millis(3));
        assert_eq!(m.poll(), IdleAction::Lock);
        // Lock is announced once only.
        assert_eq!(m.poll(), IdleAction::None);
        // Later: EndSession.
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(m.poll(), IdleAction::EndSession);
        // Terminal.
        assert_eq!(m.poll(), IdleAction::None);
    }

    #[test]
    fn leaving_idle_cancels_timers() {
        let mut m = IdleMachine::new(policy(1, 5));
        m.set_idle(true); // transition only; decisions come from poll()
        std::thread::sleep(Duration::from_millis(3));
        assert_eq!(m.poll(), IdleAction::Lock);
        // User returns: timers reset, locked flag reset (a fresh idle
        // period must lock again).
        m.set_idle(false);
        assert!(!m.is_idle());
        assert_eq!(m.poll(), IdleAction::None);
        // Second idle period locks again.
        m.set_idle(true);
        std::thread::sleep(Duration::from_millis(3));
        assert_eq!(m.poll(), IdleAction::Lock);
    }

    #[test]
    fn note_locked_prevents_double_lock() {
        let mut m = IdleMachine::new(policy(1, 0));
        m.set_idle(true);
        m.note_locked(); // e.g. loginctl lock-session got there first
        std::thread::sleep(Duration::from_millis(3));
        assert_eq!(m.poll(), IdleAction::None);
    }

    #[test]
    fn logout_without_lock_is_valid_kiosk_policy() {
        // lock disabled (0), logout enabled: the machine goes straight
        // to EndSession.
        let mut m = IdleMachine::new(policy(0, 2));
        m.set_idle(true);
        std::thread::sleep(Duration::from_millis(3));
        assert_eq!(m.poll(), IdleAction::EndSession);
    }

    #[test]
    fn reentering_idle_after_end_is_terminal() {
        let mut m = IdleMachine::new(policy(1, 2));
        m.set_idle(true);
        // (no action expected from the transition itself)
        std::thread::sleep(Duration::from_millis(4));
        assert_eq!(m.poll(), IdleAction::EndSession);
        // Even a fresh idle period does not resurrect a session that
        // already ended (the caller ends it; the machine stays done).
        m.set_idle(false);
        m.set_idle(true);
        std::thread::sleep(Duration::from_millis(4));
        assert_eq!(m.poll(), IdleAction::None);
    }
}
