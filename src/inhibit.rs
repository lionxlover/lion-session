//! The cooperative end-of-session protocol, in two registries:
//!
//! 1. **Registered clients** (org.gnome.SessionManager-style): a shell or
//!    any app calls `RegisterClient`, receives a token, and when a session
//!    end begins it gets the `QueryEndSession` signal. It answers with
//!    `EndSessionResponse(ok, message)`. An `ok=false` answer *vetoes a
//!    logout* (GNOME semantics); restart and shutdown cannot be vetoed --
//!    after the timeout they are forced.
//!
//! 2. **Inhibitors**: `Inhibit(app, reason)` returns a cookie; while any
//!    inhibitor is held, `QueryEndSession` waits for them to finish
//!    (bounded by the configured end timeout, so a wedged app can never
//!    hang the session forever -- a deliberate divergence from GNOME,
//!    which can block logout indefinitely).
//!
//! All locks are std Mutexes held only for microseconds (never across an
//! await), and always acquired in the documented order
//! (state -> clients -> responses) from a clean state, which keeps this
//! usable from async D-Bus handlers without deadlock.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::Duration,
};

#[derive(Debug, Clone)]
pub struct ClientInfo {
    pub app_id: String,
}

#[derive(Debug, Clone)]
pub struct Inhibitor {
    pub app_id: String,
    pub reason: String,
}

/// What an `EndSessionResponse` call did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseEffect {
    /// The client approved the end; recorded.
    Answered,
    /// The client vetoed (message carried).
    Veto(String),
    /// This approval was the last missing answer.
    Complete,
    /// Unknown token or no query running.
    Ignored,
}

/// Outcome of waiting for answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitOutcome {
    /// Every registered client answered ok and no inhibitors remain.
    AllAnswered,
    /// A client vetoed with this message.
    Veto(String),
    /// The deadline passed; these app_ids never answered.
    Timeout(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Querying,
    Ending,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Idle => "running",
            State::Querying => "query-end",
            State::Ending => "ending",
        }
    }
}

pub struct EndProtocol {
    state: Mutex<State>,
    clients: Mutex<HashMap<u64, ClientInfo>>,
    responses: Mutex<HashMap<u64, bool>>,
    inhibitors: Mutex<HashMap<u64, Inhibitor>>,
    next_id: AtomicU64,
    /// Set when a veto arrives; `wait_answers` returns early.
    veto: Mutex<Option<String>>,
    /// Woken when a response arrives, a client unregisters, or an
    /// inhibitor is released.
    answered: tokio::sync::Notify,
    end_timeout: Duration,
}

impl EndProtocol {
    pub fn new(end_timeout: Duration) -> Self {
        Self {
            state: Mutex::new(State::Idle),
            clients: Mutex::new(HashMap::new()),
            responses: Mutex::new(HashMap::new()),
            inhibitors: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            veto: Mutex::new(None),
            answered: tokio::sync::Notify::new(),
            end_timeout,
        }
    }

    pub fn state(&self) -> State {
        *self.state.lock().unwrap()
    }

    // ---- client registry ------------------------------------------------

    pub fn register_client(&self, app_id: &str) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.clients.lock().unwrap().insert(
            id,
            ClientInfo {
                app_id: app_id.to_string(),
            },
        );
        id
    }

    pub fn unregister_client(&self, token: u64) -> bool {
        let had = self.clients.lock().unwrap().remove(&token).is_some();
        if had {
            self.responses.lock().unwrap().remove(&token);
            self.answered.notify_waiters();
        }
        had
    }

    pub fn client_count(&self) -> usize {
        self.clients.lock().unwrap().len()
    }

    // ---- inhibitors ------------------------------------------------------

    pub fn inhibit(&self, app_id: &str, reason: &str) -> u64 {
        let cookie = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.inhibitors.lock().unwrap().insert(
            cookie,
            Inhibitor {
                app_id: app_id.to_string(),
                reason: reason.to_string(),
            },
        );
        cookie
    }

    pub fn uninhibit(&self, cookie: u64) -> bool {
        let had = self.inhibitors.lock().unwrap().remove(&cookie).is_some();
        if had {
            self.answered.notify_waiters();
        }
        had
    }

    pub fn is_inhibited(&self) -> bool {
        !self.inhibitors.lock().unwrap().is_empty()
    }

    pub fn inhibitors_snapshot(&self) -> Vec<(String, String)> {
        let mut v: Vec<_> = self
            .inhibitors
            .lock()
            .unwrap()
            .values()
            .map(|i| (i.app_id.clone(), i.reason.clone()))
            .collect();
        v.sort();
        v
    }

    // ---- end sequence ----------------------------------------------------

    /// Start a query round if anyone is listening (registered clients or
    /// inhibitors). Returns false when the end can proceed immediately.
    pub fn begin_query(&self) -> bool {
        let needed = self.client_count() > 0 || self.is_inhibited();
        if needed {
            {
                let mut st = self.state.lock().unwrap();
                *st = State::Querying;
            }
            self.responses.lock().unwrap().clear();
            *self.veto.lock().unwrap() = None;
        }
        needed
    }

    /// Record one client answer. Idempotent per token; a veto overrides
    /// every earlier approval.
    pub fn record_response(&self, token: u64, ok: bool, message: &str) -> ResponseEffect {
        if self.state() != State::Querying {
            return ResponseEffect::Ignored;
        }
        let effect = {
            let clients = self.clients.lock().unwrap();
            if !clients.contains_key(&token) {
                return ResponseEffect::Ignored;
            }
            if !ok {
                ResponseEffect::Veto(message.to_string())
            } else {
                let responses = self.responses.lock().unwrap();
                let total = if responses.contains_key(&token) {
                    responses.len()
                } else {
                    responses.len() + 1
                };
                if total >= clients.len() {
                    ResponseEffect::Complete
                } else {
                    ResponseEffect::Answered
                }
            }
        };
        if let ResponseEffect::Veto(msg) = &effect {
            *self.veto.lock().unwrap() = Some(msg.clone());
        } else {
            self.responses.lock().unwrap().insert(token, ok);
        }
        self.answered.notify_waiters();
        effect
    }

    /// Wait until every registered client answered ok and no inhibitors
    /// remain, a veto arrives, or the deadline (the configured end
    /// timeout) expires.
    pub async fn wait_answers(&self) -> WaitOutcome {
        let deadline = tokio::time::Instant::now() + self.end_timeout;
        loop {
            if let Some(v) = self.veto.lock().unwrap().clone() {
                return WaitOutcome::Veto(v);
            }
            let (pending, clients_left, inhibited) = {
                let clients = self.clients.lock().unwrap();
                let responses = self.responses.lock().unwrap();
                let pending: Vec<String> = clients
                    .iter()
                    .filter(|(id, _)| !responses.contains_key(id))
                    .map(|(_, c)| c.app_id.clone())
                    .collect();
                (pending, !clients.is_empty(), self.is_inhibited())
            };
            if pending.is_empty() && (clients_left || !inhibited) {
                // all clients answered, or nobody is listening at all
                return WaitOutcome::AllAnswered;
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return WaitOutcome::Timeout(pending);
            }
            let _ = tokio::time::timeout_at(deadline, self.answered.notified()).await;
        }
    }

    /// Move to Ending; the end will now happen.
    pub fn finish(&self) {
        *self.state.lock().unwrap() = State::Ending;
    }

    /// Abort a query round (veto path): back to running state.
    pub fn cancel(&self) {
        *self.state.lock().unwrap() = State::Idle;
        self.responses.lock().unwrap().clear();
        *self.veto.lock().unwrap() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proto() -> EndProtocol {
        EndProtocol::new(Duration::from_millis(50))
    }

    #[test]
    fn ids_are_unique_across_registries() {
        let p = proto();
        let c1 = p.register_client("a");
        let i1 = p.inhibit("a", "saving");
        let c2 = p.register_client("b");
        assert!(c1 != i1 && i1 != c2 && c1 != c2);
    }

    #[test]
    fn query_not_needed_when_nobody_listens() {
        let p = proto();
        assert!(!p.begin_query());
        assert_eq!(p.state(), State::Idle);
    }

    #[test]
    fn query_needed_for_clients_and_inhibitors() {
        let p = proto();
        let _ = p.register_client("shell");
        assert!(p.begin_query());
        p.cancel();
        let _ = p.inhibit("app", "saving");
        assert!(p.begin_query());
    }

    #[tokio::test]
    async fn veto_short_circuits_wait() {
        let p = proto();
        let t = p.register_client("shell");
        assert!(p.begin_query());
        assert_eq!(
            p.record_response(t, false, "busy"),
            ResponseEffect::Veto("busy".into())
        );
        assert_eq!(p.wait_answers().await, WaitOutcome::Veto("busy".into()));
        p.cancel();
        assert_eq!(p.state(), State::Idle);
    }

    #[tokio::test]
    async fn all_answered_completes() {
        let p = proto();
        let a = p.register_client("a");
        let b = p.register_client("b");
        assert!(p.begin_query());
        assert_eq!(p.record_response(a, true, ""), ResponseEffect::Answered);
        let effects = p.record_response(b, true, "");
        assert!(matches!(
            effects,
            ResponseEffect::Complete | ResponseEffect::Answered
        ));
        let out = tokio::time::timeout(Duration::from_secs(1), p.wait_answers())
            .await
            .unwrap();
        assert_eq!(out, WaitOutcome::AllAnswered);
    }

    #[tokio::test]
    async fn idempotent_responses_do_not_fake_completion() {
        let p = proto();
        let a = p.register_client("a");
        let b = p.register_client("b");
        assert!(p.begin_query());
        p.record_response(a, true, "");
        p.record_response(a, true, ""); // repeat must not count twice
        assert_eq!(p.record_response(b, true, ""), ResponseEffect::Complete);
    }

    #[tokio::test]
    async fn timeout_reports_pending_apps() {
        let p = EndProtocol::new(Duration::from_millis(30));
        let _ = p.register_client("slow-app");
        let _ = p.inhibit("save-daemon", "writing files");
        assert!(p.begin_query());
        match p.wait_answers().await {
            WaitOutcome::Timeout(pending) => assert_eq!(pending, vec!["slow-app".to_string()]),
            other => panic!("expected timeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn releasing_last_inhibitor_unblocks_wait() {
        let p = EndProtocol::new(Duration::from_secs(10));
        let cookie = p.inhibit("save-daemon", "writing files");
        assert!(p.begin_query());
        p.uninhibit(cookie);
        let out = tokio::time::timeout(Duration::from_secs(1), p.wait_answers())
            .await
            .unwrap();
        assert_eq!(out, WaitOutcome::AllAnswered);
    }

    #[test]
    fn unknown_or_late_answers_are_ignored() {
        let p = proto();
        assert_eq!(p.record_response(999, true, ""), ResponseEffect::Ignored);
        let t = p.register_client("a");
        // no query running
        assert_eq!(p.record_response(t, true, ""), ResponseEffect::Ignored);
    }

    #[test]
    fn uninhibit_and_unregister() {
        let p = proto();
        let c = p.register_client("a");
        let i = p.inhibit("a", "r");
        assert!(p.is_inhibited());
        assert!(p.uninhibit(i));
        assert!(!p.is_inhibited());
        assert!(!p.uninhibit(i)); // double release rejected
        assert!(p.unregister_client(c));
        assert!(!p.unregister_client(c));
        assert_eq!(p.client_count(), 0);
    }

    #[test]
    fn inhibitors_snapshot_sorted() {
        let p = proto();
        let _ = p.inhibit("b-app", "r2");
        let _ = p.inhibit("a-app", "r1");
        assert_eq!(
            p.inhibitors_snapshot(),
            vec![("a-app".into(), "r1".into()), ("b-app".into(), "r2".into())]
        );
    }
}
