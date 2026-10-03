#![forbid(unsafe_code)]
//! Session end state machine (spec 02 §3 lifecycle actions).
//!
//! Pure logic: transitions take an explicit `now` and return a list of
//! [`Output`]s (signals to emit, actions to execute) that the orchestration
//! layer applies. Graceful shutdown shape: apps registered via
//! `RegisterClient` get `QueryEndSession`, everyone acks or the
//! `shutdown_timeout_ms` deadline expires, then `EndSession` fires, apps
//! are force-killed and the final action runs through logind.
//!
//! Inhibitors are consulted before a lifecycle action starts (an app can
//! block shutdown with a reason); inhibitors that appear mid-query simply
//! hold the query until the same deadline — the timeout is the backstop.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

/// Lifecycle actions (spec 02 §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Logout,
    Restart,
    Shutdown,
    Suspend,
    Hibernate,
}

impl Action {
    /// Human/bus-facing name.
    pub fn name(&self) -> &'static str {
        match self {
            Action::Logout => "logout",
            Action::Restart => "restart",
            Action::Shutdown => "shutdown",
            Action::Suspend => "suspend",
            Action::Hibernate => "hibernate",
        }
    }

    /// Does ending this action end the *session* (vs just the machine)?
    pub fn ends_session(&self) -> bool {
        matches!(self, Action::Logout | Action::Restart | Action::Shutdown)
    }

    /// Query clients first, or execute directly.
    pub fn queries_clients(&self) -> bool {
        matches!(self, Action::Logout | Action::Restart | Action::Shutdown)
    }
}

/// Inhibitor classes that block each action (spec: what ∈
/// {logout, shutdown, suspend, idle, switch-user}).
pub fn action_inhibitor_class(a: Action) -> &'static str {
    match a {
        Action::Logout => "logout",
        Action::Restart | Action::Shutdown => "shutdown",
        Action::Suspend | Action::Hibernate => "suspend",
    }
}

/// Signals / side-effects the machine asks the caller to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// Emit QueryEndSession(flags).
    SignalQueryEndSession(u32),
    /// Emit EndSession(flags).
    SignalEndSession(u32),
    /// Persist session state (opt-in restore) before tearing down.
    PersistState,
    /// Stop services (reverse plan order) and kill the compositor.
    ForceKill,
    /// Execute the action through logind.
    ExecuteAction(Action),
}

/// QueryEndSession flag bits (documented in docs/DBUS.md).
pub const FLAG_FORCE: u32 = 1;
pub const FLAG_RESTART: u32 = 2;

/// Snapshot of one inhibitor relevant to a decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InhibitorSnapshot {
    pub what: String,
    pub who: String,
    pub why: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Running,
    QueryEnd { deadline: Instant },
    Ended,
}

/// The machine.
#[derive(Debug)]
pub struct Machine {
    phase: Phase,
    action: Option<Action>,
    deadline: Option<Instant>,
    registered: BTreeSet<String>,
    pending: BTreeSet<String>,
    timeout: Duration,
}

impl Machine {
    pub fn new(timeout: Duration) -> Machine {
        Machine {
            phase: Phase::Running,
            action: None,
            deadline: None,
            registered: BTreeSet::new(),
            pending: BTreeSet::new(),
            timeout,
        }
    }

    /// D-Bus `State` property value.
    pub fn state(&self) -> &'static str {
        match self.phase {
            Phase::Running => "running",
            Phase::QueryEnd { .. } => "query-end-session",
            Phase::Ended => "ended",
        }
    }

    pub fn is_running(&self) -> bool {
        self.phase == Phase::Running
    }

    /// Register an app for end-session queries. Only valid while Running.
    pub fn register(&mut self, app_id: &str) -> Result<(), String> {
        if app_id.is_empty() {
            return Err("app_id must not be empty".into());
        }
        if !self.is_running() {
            return Err("session is ending; registration refused".into());
        }
        self.registered.insert(app_id.to_string());
        Ok(())
    }

    /// A registered client disconnected from the bus: it can never ack,
    /// so treat it as satisfied (spec 02 §6 inhibitor-leak analog).
    pub fn client_gone(&mut self, app_id: &str) -> Vec<Output> {
        self.pending.remove(app_id);
        self.drain_if_complete()
    }

    /// Client acknowledged the query.
    pub fn ack(&mut self, app_id: &str) -> Vec<Output> {
        self.pending.remove(app_id);
        self.drain_if_complete()
    }

    /// Attempt to start a lifecycle action. `inhibitors` is the caller's
    /// current inhibitor snapshot; entries blocking this action refuse the
    /// call with the blocker's identity (fail closed).
    pub fn try_begin(
        &mut self,
        action: Action,
        now: Instant,
        inhibitors: &[InhibitorSnapshot],
    ) -> Result<Vec<Output>, String> {
        if !matches!(self.phase, Phase::Running) {
            return Err(format!("session is {} — action refused", self.state()));
        }
        let class = action_inhibitor_class(action);
        if let Some(b) = inhibitors.iter().find(|i| i.what == class) {
            return Err(format!("inhibited by {}: {}", b.who, b.why));
        }
        if !action.queries_clients() {
            // Suspend/hibernate: execute directly through logind.
            return Ok(vec![Output::ExecuteAction(action)]);
        }
        self.action = Some(action);
        self.deadline = Some(now + self.timeout);
        self.pending = self.registered.clone();
        self.phase = Phase::QueryEnd {
            deadline: now + self.timeout,
        };
        let mut flags = 0u32;
        if action == Action::Restart {
            flags |= FLAG_RESTART;
        }
        if self.pending.is_empty() {
            // No registered clients to ask: end immediately (the query
            // signal still fires for bus listeners).
            let mut out = vec![Output::SignalQueryEndSession(flags)];
            out.extend(self.complete(flags));
            return Ok(out);
        }
        Ok(vec![Output::SignalQueryEndSession(flags)])
    }

    /// Deadline tick: force the end when the grace period expired.
    pub fn tick(&mut self, now: Instant) -> Vec<Output> {
        if let Phase::QueryEnd { deadline } = self.phase {
            if now >= deadline {
                return self.force_end();
            }
        }
        Vec::new()
    }

    /// Registered clients that have not acked yet (the "which app is
    /// blocking" surface).
    pub fn pending(&self) -> Vec<String> {
        self.pending.iter().cloned().collect()
    }

    /// Active action, if a lifecycle flow is in progress.
    pub fn action(&self) -> Option<Action> {
        self.action
    }

    /// Force the end regardless of acks (timeout backstop / compositor
    /// death path).
    pub fn force_end(&mut self) -> Vec<Output> {
        if matches!(self.phase, Phase::Ended) {
            return Vec::new();
        }
        let action = self.action;
        self.pending.clear();
        self.phase = Phase::Ended;
        let mut out = vec![
            Output::SignalEndSession(FLAG_FORCE),
            Output::PersistState,
            Output::ForceKill,
        ];
        if let Some(a) = action {
            out.push(Output::ExecuteAction(a));
        }
        out
    }

    fn drain_if_complete(&mut self) -> Vec<Output> {
        let complete = matches!(self.phase, Phase::QueryEnd { .. }) && self.pending.is_empty();
        if !complete {
            return Vec::new();
        }
        let mut flags = 0u32;
        if self.action == Some(Action::Restart) {
            flags |= FLAG_RESTART;
        }
        self.complete(flags)
    }

    /// Transition to Ended and produce the final outputs.
    fn complete(&mut self, flags: u32) -> Vec<Output> {
        let action = self.action;
        self.phase = Phase::Ended;
        let mut out = vec![
            Output::SignalEndSession(flags),
            Output::PersistState,
            Output::ForceKill,
        ];
        if let Some(a) = action {
            out.push(Output::ExecuteAction(a));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(ms: u64) -> Instant {
        Instant::now() + Duration::from_millis(ms)
    }

    #[test]
    fn logout_queries_registered_clients() {
        let mut m = Machine::new(Duration::from_millis(8000));
        m.register("app-a").unwrap();
        m.register("app-b").unwrap();
        let out = m.try_begin(Action::Logout, t(0), &[]).unwrap();
        assert_eq!(out, vec![Output::SignalQueryEndSession(0)]);
        assert_eq!(m.state(), "query-end-session");
        assert_eq!(m.pending(), vec!["app-a", "app-b"]);
    }

    #[test]
    fn all_acks_end_session() {
        let mut m = Machine::new(Duration::from_millis(8000));
        m.register("app-a").unwrap();
        m.try_begin(Action::Logout, t(0), &[]).unwrap();
        let out = m.ack("app-a");
        assert_eq!(
            out,
            vec![
                Output::SignalEndSession(0),
                Output::PersistState,
                Output::ForceKill,
                Output::ExecuteAction(Action::Logout),
            ]
        );
        assert_eq!(m.state(), "ended");
    }

    #[test]
    fn timeout_forces_with_flag() {
        let mut m = Machine::new(Duration::from_millis(1000));
        m.register("app-a").unwrap();
        m.try_begin(Action::Logout, t(0), &[]).unwrap();
        // before the deadline: nothing
        assert!(m.tick(t(999)).is_empty());
        let out = m.tick(t(1001));
        assert_eq!(out.first(), Some(&Output::SignalEndSession(FLAG_FORCE)));
        assert!(out.contains(&Output::ExecuteAction(Action::Logout)));
        assert_eq!(m.state(), "ended");
    }

    #[test]
    fn inhibitors_block_the_action() {
        let mut m = Machine::new(Duration::from_millis(8000));
        let inh = vec![InhibitorSnapshot {
            what: "shutdown".into(),
            who: "lion-text".into(),
            why: "unsaved document".into(),
        }];
        let e = m.try_begin(Action::Shutdown, t(0), &inh).unwrap_err();
        assert!(e.contains("lion-text"), "got: {e}");
        assert!(e.contains("unsaved document"));
        // logout is a different class and proceeds
        assert!(m.try_begin(Action::Logout, t(0), &inh).is_ok());
    }

    #[test]
    fn suspend_executes_directly_without_query() {
        let mut m = Machine::new(Duration::from_millis(8000));
        m.register("app-a").unwrap();
        let out = m.try_begin(Action::Suspend, t(0), &[]).unwrap();
        assert_eq!(out, vec![Output::ExecuteAction(Action::Suspend)]);
        assert_eq!(m.state(), "running");
    }

    #[test]
    fn restart_sets_restart_flag() {
        let mut m = Machine::new(Duration::from_millis(8000));
        m.register("app").unwrap();
        let out = m.try_begin(Action::Restart, t(0), &[]).unwrap();
        assert_eq!(out, vec![Output::SignalQueryEndSession(FLAG_RESTART)]);
        let out = m.ack("app");
        assert_eq!(out.first(), Some(&Output::SignalEndSession(FLAG_RESTART)));
    }

    #[test]
    fn logout_without_clients_completes_immediately() {
        let mut m = Machine::new(Duration::from_millis(8000));
        let out = m.try_begin(Action::Logout, t(0), &[]).unwrap();
        assert_eq!(
            out,
            vec![
                Output::SignalQueryEndSession(0),
                Output::SignalEndSession(0),
                Output::PersistState,
                Output::ForceKill,
                Output::ExecuteAction(Action::Logout),
            ]
        );
        assert_eq!(m.state(), "ended");
    }

    #[test]
    fn second_action_refused_while_ending() {
        let mut m = Machine::new(Duration::from_millis(8000));
        m.try_begin(Action::Logout, t(0), &[]).unwrap();
        let e = m.try_begin(Action::Shutdown, t(1), &[]).unwrap_err();
        assert!(e.contains("refused"));
    }

    #[test]
    fn vanished_client_counts_as_acked() {
        let mut m = Machine::new(Duration::from_millis(8000));
        m.register("app-a").unwrap();
        m.try_begin(Action::Logout, t(0), &[]).unwrap();
        let out = m.client_gone("app-a");
        assert!(out.contains(&Output::ExecuteAction(Action::Logout)));
        assert_eq!(m.state(), "ended");
    }

    #[test]
    fn register_refused_during_query() {
        let mut m = Machine::new(Duration::from_millis(8000));
        m.try_begin(Action::Logout, t(0), &[]).unwrap();
        assert!(m.register("late-app").is_err());
    }

    #[test]
    fn register_rejects_empty_app_id() {
        let mut m = Machine::new(Duration::from_millis(8000));
        assert!(m.register("").is_err());
    }

    #[test]
    fn force_end_is_idempotent() {
        let mut m = Machine::new(Duration::from_millis(8000));
        m.register("app-a").unwrap();
        m.try_begin(Action::Logout, t(0), &[]).unwrap();
        let first = m.force_end();
        assert!(!first.is_empty());
        assert!(m.force_end().is_empty());
    }

    #[test]
    fn unregistered_ack_is_ignored() {
        let mut m = Machine::new(Duration::from_millis(8000));
        m.register("app-a").unwrap();
        m.try_begin(Action::Logout, t(0), &[]).unwrap();
        let out = m.ack("ghost-app");
        assert!(out.is_empty());
        assert_eq!(m.state(), "query-end-session");
        assert_eq!(m.pending(), vec!["app-a"]);
    }

    #[test]
    fn action_classes() {
        assert_eq!(action_inhibitor_class(Action::Restart), "shutdown");
        assert_eq!(action_inhibitor_class(Action::Hibernate), "suspend");
        assert!(Action::Logout.ends_session());
        assert!(!Action::Suspend.ends_session());
        assert!(!Action::Suspend.queries_clients());
    }
}
