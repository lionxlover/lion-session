//! D-Bus surface exposed to the shell and apps (session bus).
//!
//! Bus name    : os.lionos.Session
//! Object path : /os/lionos/Session
//! Interface   : os.lionos.Session1
//!
//! Methods
//!   Logout() / Restart() / Shutdown()   end the session (cooperatively;
//!                                       see QueryEndSession below)
//!   Suspend() / Hibernate()             forward to lion-power; the
//!                                       session keeps running
//!   Lock() / Unlock()                   forward to lion-locker; deciding
//!                                       *when* to lock is lion-idle's job
//!   RegisterClient(app_id) -> token     join the end-of-session protocol
//!   UnregisterClient(token)
//!   EndSessionResponse(token, ok, msg)  answer a QueryEndSession; ok=false
//!                                       vetoes a *logout* (GNOME
//!                                       semantics), not a power action
//!   Inhibit(app_id, reason) -> cookie   delay an end while saving state
//!   Uninhibit(cookie) / IsInhibited() / ListInhibitors()
//!   SetIdle(idle)                       push the idle hint to logind
//!   GetCapabilities() -> Vec<String>    feature discovery for shells
//!   GetMetrics() -> String              JSON telemetry snapshot
//!
//! Signals
//!   QueryEndSession(s reason)           "save your state now"; apps answer
//!                                       EndSessionResponse; bounded by
//!                                       [session] end-timeout-ms
//!   EndSession(s reason)                the end is happening for real
//!   EndCanceled(s message)              a client vetoed a logout
//!   PreparingToEnd(s reason)            teardown imminent ("logout" |
//!                                       "restart" | "shutdown"); the shell
//!                                       may fade out now
//!   IdleChanged(b idle)
//!   InhibitorAdded(u cookie, s app, s reason) / InhibitorRemoved(u cookie)
//!   CompositorRestarted(u attempt)      the compositor crashed and was
//!                                       respawned; re-query outputs
//!
//! Properties (read-only)
//!   State (s) -- running | query-end | ending; changes are announced on
//!   the standard PropertiesChanged signal
//!
//! Every method returns immediately; the end sequence runs as a background
//! task so the D-Bus reply itself never races the teardown it announces.
//! Restart/Shutdown additionally take a logind *delay* inhibitor for the
//! whole teardown window, so the system cannot finish powering off
//! underneath a session that is still saving state.

use crate::{
    config::Services,
    idle::IdleMachine,
    inhibit::{EndProtocol, WaitOutcome},
    logind::Logind,
    metrics::{Counter, Metrics},
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use tokio::sync::mpsc::UnboundedSender;
use zbus::{
    interface,
    object_server::{InterfaceRef, SignalContext},
    Connection,
};

/// One end-of-session request, as consumed by the session supervisor.
pub struct EndRequest {
    pub reason: EndReason,
    /// logind delay inhibitor held for the teardown window; dropped when
    /// teardown completes (that closes the fd, which releases logind).
    pub inhibitor: Option<zbus::zvariant::OwnedFd>,
    /// True for fast paths (logind PrepareForShutdown, signals): skip the
    /// logout animation, the system is waiting.
    pub fast: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    Logout,
    Restart,
    Shutdown,
}

impl EndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            EndReason::Logout => "logout",
            EndReason::Restart => "restart",
            EndReason::Shutdown => "shutdown",
        }
    }
}

/// The full advertised feature set (also used by the README table).
pub const CAPABILITIES: &[&str] = &[
    "logout",
    "restart",
    "shutdown",
    "suspend",
    "hibernate",
    "lock",
    "unlock",
    "query-end",
    "inhibit",
    "idle-hint",
    "logind",
    "xdg-autostart",
    "systemd-scope",
    "metrics",
    "compositor-recovery",
    "idle-escalation",
    "lock-on-shutdown",
    "resource-limits",
    "orphan-cleanup",
];

pub struct SessionService {
    services: Services,
    lock_on_sleep: bool,
    /// 0.3.0: lock the moment logind announces PrepareForShutdown — a
    /// shutdown can still be cancelled, and a cancelled shutdown must
    /// leave a locked session, not an exposed one.
    lock_on_shutdown: bool,
    /// 0.3.0: idle escalation machine, driven by SetIdle + the 1 s
    /// tick task (see `session.rs`). Shared so both writers (D-Bus
    /// method, logind events) feed the same policy.
    idle: Arc<Mutex<IdleMachine>>,
    /// Session bus connection used to forward Lock/Restart/... to
    /// lion-locker / lion-power. Separate from the connection this
    /// interface is served on so forwarding failures never affect it.
    peer_bus: Connection,
    end_requested: Arc<AtomicBool>,
    end_tx: UnboundedSender<EndRequest>,
    protocol: Arc<EndProtocol>,
    metrics: Arc<Metrics>,
    logind: Option<Logind>,
    state: Arc<Mutex<String>>,
    /// Signal context built once the serving connection exists.
    sig_ctxt: tokio::sync::OnceCell<SignalContext<'static>>,
}

impl SessionService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        services: Services,
        lock_on_sleep: bool,
        lock_on_shutdown: bool,
        idle_policy: crate::idle::IdlePolicy,
        peer_bus: Connection,
        end_tx: UnboundedSender<EndRequest>,
        protocol: Arc<EndProtocol>,
        metrics: Arc<Metrics>,
        logind: Option<Logind>,
    ) -> Self {
        Self {
            services,
            lock_on_sleep,
            lock_on_shutdown,
            idle: Arc::new(Mutex::new(IdleMachine::new(idle_policy))),
            peer_bus,
            end_requested: Arc::new(AtomicBool::new(false)),
            end_tx,
            protocol,
            metrics,
            logind,
            state: Arc::new(Mutex::new("starting".into())),
            sig_ctxt: tokio::sync::OnceCell::new(),
        }
    }

    /// The shared idle machine, for the tick task in `session.rs`.
    pub fn idle_machine(&self) -> Arc<Mutex<IdleMachine>> {
        self.idle.clone()
    }

    /// Called once the serving connection is live, so background tasks
    /// have a signal context of their own.
    pub async fn attach(&self, conn: &Connection, path: &str) {
        if let Ok(ctxt) = SignalContext::new(conn, path.to_string()) {
            let _ = self.sig_ctxt.set(ctxt);
        }
        self.set_state("running").await;
        crate::sdnotify::ready();
    }

    fn ctxt(&self) -> Option<SignalContext<'static>> {
        self.sig_ctxt.get().cloned()
    }

    async fn set_state(&self, value: &str) {
        *self.state.lock().unwrap() = value.to_string();
        if let Some(ctxt) = self.ctxt() {
            // Uses the property-generated `state_changed`, which emits the
            // standard org.freedesktop.DBus.Properties.PropertiesChanged.
            let _ = Self::state_changed(self, &ctxt).await;
        }
    }

    /// State update from a task that holds no `&self` (the veto path):
    /// writes the shared cell and emits PropertiesChanged manually.
    async fn set_state_from_task(
        state: &std::sync::Arc<Mutex<String>>,
        value: &str,
        ctxt: Option<&SignalContext<'static>>,
    ) {
        *state.lock().unwrap() = value.to_string();
        let Some(ctxt) = ctxt else { return };
        let Ok(iface) = zbus::names::InterfaceName::try_from("os.lionos.Session1") else {
            return;
        };
        let value = zbus::zvariant::Value::new(value.to_string());
        let changed: std::collections::HashMap<&str, &zbus::zvariant::Value> =
            [("State", &value)].into();
        let _ = zbus::fdo::Properties::properties_changed(ctxt, iface, &changed, &[]).await;
    }

    /// Lock (or unlock) forward -- also driven by logind sleep/session
    /// signals from the session supervisor.
    pub async fn forward_lock(&self, lock: bool) {
        self.metrics.inc(Counter::LockRequests);
        let s = self.services.clone();
        let method = if lock { "Lock" } else { "Unlock" };
        self.call_forward(
            &s.locker_service,
            &s.locker_path,
            &s.locker_interface,
            method,
        )
        .await;
    }

    async fn call_forward(&self, service: &str, path: &str, interface: &str, method: &str) {
        let res = self
            .peer_bus
            .call_method(Some(service), path, Some(interface), method, &())
            .await;
        match res {
            Ok(_) => tracing::debug!(service, method, "forwarded"),
            Err(e) => tracing::warn!(service, method, error = %e, "forwarding call failed"),
        }
    }

    /// Cooperative end: query clients, hold a logind delay inhibitor for
    /// the teardown window, then hand the (inhibitor-carrying) request to
    /// the session supervisor. Returns immediately.
    async fn begin_end(&self, reason: EndReason) {
        if self.end_requested.swap(true, Ordering::SeqCst) {
            return; // an end sequence is already in flight
        }
        let need_query = self.protocol.begin_query();
        if need_query {
            self.metrics.inc(Counter::QueryEnds);
            self.set_state("query-end").await;
            if let Some(ctxt) = self.ctxt() {
                let _ = Self::query_end_session(&ctxt, reason.as_str()).await;
            }
        }

        let protocol = self.protocol.clone();
        let metrics = self.metrics.clone();
        let logind = self.logind.clone();
        let end_tx = self.end_tx.clone();
        let ctxt = self.ctxt();
        let state = self.state.clone();
        let end_requested = self.end_requested.clone();
        let can_veto = reason == EndReason::Logout;
        let reason_str = reason.as_str().to_string();

        tokio::spawn(async move {
            // Delay inhibitor for power-related ends: keeps the machine up
            // while we tear the session down cleanly.
            let inhibitor = match (&logind, reason) {
                (Some(l), EndReason::Restart | EndReason::Shutdown) => {
                    let fd = l
                        .inhibit_delay("shutdown", "session teardown in progress")
                        .await;
                    if fd.is_some() {
                        metrics.inc(Counter::LogindInhibitorHolds);
                    }
                    fd
                }
                _ => None,
            };

            if need_query {
                match protocol.wait_answers().await {
                    WaitOutcome::AllAnswered => {}
                    WaitOutcome::Veto(message) => {
                        if can_veto {
                            metrics.inc(Counter::VetoedEnds);
                            protocol.cancel();
                            end_requested.store(false, Ordering::SeqCst);
                            tracing::info!(%message, "logout vetoed by client, continuing session");
                            if let Some(ctxt) = ctxt.as_ref() {
                                let _ = Self::end_canceled(ctxt, &message).await;
                            }
                            Self::set_state_from_task(&state, "running", ctxt.as_ref()).await;
                            return; // inhibitor (if any) dropped here
                        }
                        metrics.inc(Counter::ForcedEnds);
                        tracing::warn!(%message, "client tried to veto {reason_str}, forcing");
                    }
                    WaitOutcome::Timeout(pending) => {
                        metrics.inc(Counter::ForcedEnds);
                        tracing::warn!(?pending, "apps did not answer in time, forcing end");
                    }
                }
                protocol.finish();
                if let Some(ctxt) = ctxt.as_ref() {
                    let _ = Self::end_session(ctxt, &reason_str).await;
                }
            }

            if let Some(ctxt) = ctxt.as_ref() {
                let _ = Self::preparing_to_end(ctxt, &reason_str).await;
            }
            protocol.finish();
            let _ = end_tx.send(EndRequest {
                reason,
                inhibitor,
                fast: false,
            });
        });
    }

    /// Fast path for signal-driven ends: logind is about to power off, or
    /// we got SIGTERM -- no queries, no animation, end now.
    pub async fn force_end(&self, reason: EndReason) {
        if self.end_requested.swap(true, Ordering::SeqCst) {
            return;
        }
        self.set_state("ending").await;
        let reason_str = reason.as_str();
        let inhibitor = match (&self.logind, reason) {
            (Some(l), EndReason::Restart | EndReason::Shutdown) => {
                let fd = l
                    .inhibit_delay("shutdown", "session teardown in progress")
                    .await;
                if fd.is_some() {
                    self.metrics.inc(Counter::LogindInhibitorHolds);
                }
                fd
            }
            _ => None,
        };
        if let Some(ctxt) = self.ctxt() {
            let _ = Self::preparing_to_end(&ctxt, reason_str).await;
        }
        let _ = self.end_tx.send(EndRequest {
            reason,
            inhibitor,
            fast: true,
        });
    }

    /// logind says the system is suspending: lock first if configured.
    pub async fn handle_sleep(&self, start: bool) {
        if start && self.lock_on_sleep {
            tracing::info!("system suspending, locking session first");
            self.forward_lock(true).await;
        }
    }

    /// Compositor crash-recovery notification for shells.
    pub async fn emit_compositor_restarted(&self, attempt: u32) {
        if let Some(ctxt) = self.ctxt() {
            let _ = Self::compositor_restarted(&ctxt, attempt).await;
        }
    }
}

#[interface(name = "os.lionos.Session1")]
impl SessionService {
    /// End the session cleanly (stop apps + compositor).
    async fn logout(&self) {
        self.begin_end(EndReason::Logout).await;
    }

    /// Forward to lion-power, then end the session.
    async fn restart(&self) {
        let s = self.services.clone();
        self.call_forward(
            &s.power_service,
            &s.power_path,
            &s.power_interface,
            "Restart",
        )
        .await;
        self.begin_end(EndReason::Restart).await;
    }

    /// Forward to lion-power, then end the session.
    async fn shutdown(&self) {
        let s = self.services.clone();
        self.call_forward(
            &s.power_service,
            &s.power_path,
            &s.power_interface,
            "Shutdown",
        )
        .await;
        self.begin_end(EndReason::Shutdown).await;
    }

    /// Forward to lion-power; the session itself keeps running.
    async fn suspend(&self) {
        let s = self.services.clone();
        self.call_forward(
            &s.power_service,
            &s.power_path,
            &s.power_interface,
            "Suspend",
        )
        .await;
    }

    /// Forward to lion-power; the session itself keeps running.
    async fn hibernate(&self) {
        let s = self.services.clone();
        self.call_forward(
            &s.power_service,
            &s.power_path,
            &s.power_interface,
            "Hibernate",
        )
        .await;
    }

    /// Execute a lock request; deciding *when* to lock (idle timeout,
    /// lid close, ...) is lion-idle's job, not this daemon's.
    async fn lock(&self) {
        self.forward_lock(true).await;
    }

    async fn unlock(&self) {
        self.forward_lock(false).await;
    }

    // ---- end-of-session protocol ---------------------------------------

    /// Join the cooperative end protocol. Returns a token to use with
    /// `EndSessionResponse`.
    async fn register_client(&self, app_id: &str) -> u64 {
        let token = self.protocol.register_client(app_id);
        self.metrics.inc(Counter::ClientRegistrations);
        tracing::info!(app_id, token, "client registered for end protocol");
        token
    }

    async fn unregister_client(&self, token: u64) -> bool {
        self.protocol.unregister_client(token)
    }

    /// Answer a `QueryEndSession`. `ok=false` vetoes a logout (with
    /// `message` shown to the user by the shell); power actions are
    /// forced after the timeout regardless. Returns the state after the
    /// answer ("query-end", "running" if the veto cancelled it, ...).
    async fn end_session_response(&self, token: u64, ok: bool, message: &str) -> String {
        self.protocol.record_response(token, ok, message);
        let state = self.protocol.state().as_str().to_string();
        if !ok {
            tracing::info!(token, message, "client answered veto");
        }
        state
    }

    async fn inhibit(&self, app_id: &str, reason: &str) -> u64 {
        let cookie = self.protocol.inhibit(app_id, reason);
        self.metrics.inc(Counter::SessionInhibitors);
        if let Some(ctxt) = self.ctxt() {
            let _ = Self::inhibitor_added(&ctxt, cookie, app_id, reason).await;
        }
        cookie
    }

    async fn uninhibit(&self, cookie: u64) -> bool {
        let had = self.protocol.uninhibit(cookie);
        if had {
            if let Some(ctxt) = self.ctxt() {
                let _ = Self::inhibitor_removed(&ctxt, cookie).await;
            }
        }
        had
    }

    async fn is_inhibited(&self) -> bool {
        self.protocol.is_inhibited()
    }

    async fn list_inhibitors(&self) -> Vec<(String, String)> {
        self.protocol.inhibitors_snapshot()
    }

    /// Push our idle state to logind (SetIdleHint), tell listeners,
    /// and (0.3.0) feed the escalation machine. Escalation decisions
    /// themselves are made by the tick task's `poll()` — this method
    /// only records the state transition.
    async fn set_idle(&self, idle: bool) {
        self.metrics.inc(Counter::IdleHints);
        self.idle.lock().unwrap().set_idle(idle);
        if let Some(l) = &self.logind {
            l.set_idle_hint(idle).await;
        }
        if let Some(ctxt) = self.ctxt() {
            let _ = Self::idle_changed(&ctxt, idle).await;
        }
    }

    /// Idle escalation: lock (called by the tick task too).
    pub async fn escalate_lock(&self) {
        self.metrics.inc(Counter::IdleLocks);
        self.forward_lock(true).await;
    }

    /// Idle escalation: end the session through the normal cooperative
    /// path (kiosk logout) — not a kill, the protocol still runs.
    pub async fn escalate_end(&self) {
        self.metrics.inc(Counter::IdleLogouts);
        tracing::info!("idle logout policy reached, ending session");
        let _ = self.end_tx.send(EndRequest {
            reason: EndReason::Logout,
            inhibitor: None,
            fast: false,
        });
    }

    /// logind PrepareForShutdown handler: lock first (0.3.0) — the
    /// shutdown may still be cancelled by another inhibitor, and a
    /// cancelled shutdown must leave the screen locked — then end.
    pub async fn shutdown_imminent(&self) {
        if self.lock_on_shutdown {
            self.metrics.inc(Counter::ShutdownLocks);
            self.forward_lock(true).await;
        }
    }

    async fn get_capabilities(&self) -> Vec<String> {
        CAPABILITIES.iter().map(|s| s.to_string()).collect()
    }

    async fn get_metrics(&self) -> String {
        self.metrics.snapshot_json()
    }

    /// Read-only property: running | query-end | ending. Changes are
    /// announced through the standard
    /// org.freedesktop.DBus.Properties.PropertiesChanged signal.
    #[zbus(property)]
    async fn state(&self) -> String {
        self.state.lock().unwrap().clone()
    }

    // ---- signals ---------------------------------------------------------

    #[zbus(signal)]
    async fn query_end_session(ctxt: &SignalContext<'_>, reason: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn end_session(ctxt: &SignalContext<'_>, reason: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn end_canceled(ctxt: &SignalContext<'_>, message: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn preparing_to_end(ctxt: &SignalContext<'_>, reason: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn idle_changed(ctxt: &SignalContext<'_>, idle: bool) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn inhibitor_added(
        ctxt: &SignalContext<'_>,
        cookie: u64,
        app: &str,
        reason: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn inhibitor_removed(ctxt: &SignalContext<'_>, cookie: u64) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn compositor_restarted(ctxt: &SignalContext<'_>, attempt: u32) -> zbus::Result<()>;
}

/// Convenience: fetch the interface reference from a built connection.
pub async fn interface_ref(
    conn: &Connection,
    path: &str,
) -> zbus::Result<InterfaceRef<SessionService>> {
    conn.object_server()
        .interface::<_, SessionService>(path)
        .await
}
