#![forbid(unsafe_code)]
//! The `os.lionos.Session1` D-Bus service (spec 02 §4), feature
//! `real-backends`.
//!
//! Methods (all validated, authorized and rate-limited in the core before
//! acting): `Logout`, `Restart`, `Shutdown`, `Suspend`, `Hibernate`,
//! `Lock`, `SwitchUser`, `Inhibit(what, who, why) -> fd`,
//! `RegisterClient(app_id)` plus the documented LionOS extension
//! `EndSessionReply(app_id)` (apps ack the end-session query without
//! disconnecting; a disconnect also counts as an ack).
//!
//! Signals: `SessionReady`, `QueryEndSession(flags)`, `EndSession(flags)`,
//! `ServiceFailed(name, reason)`. Properties: `State`, `InhibitedActions`,
//! `SafeMode` (plus the documented extension `Blockers` — "show which app
//! is blocking"). PropertiesChanged is emitted whenever the core's
//! projections change.
//!
//! Caller identity (spec 02 §8): resolved through the bus daemon
//! (GetConnectionUnixUser/ProcessID — kernel-mediated, never
//! caller-supplied strings); the pid is pinned with a pidfd and its
//! cgroup recorded for audit logs.

use crate::config::Config;
use crate::lifecycle::Action;
use crate::session::{Event, Shared};
use crate::sysffi;
use futures_util::StreamExt;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedFd;

const PATH: &str = "/os/lionos/Session1";
const IFACE: &str = "os.lionos.Session1";

/// Caller identity resolved through the bus.
struct Identity {
    uid: u32,
    pid: u32,
    pinned_pidfd: bool,
    cgroup: Option<String>,
}

/// The interface object.
struct SessionIface {
    ev: mpsc::UnboundedSender<Event>,
    shared: Shared,
}

impl SessionIface {
    /// Resolve a caller through the bus daemon. Only the bus-assigned
    /// sender name is trusted — never client-controlled message fields.
    async fn identity_of(
        &self,
        conn: &zbus::Connection,
        sender: Option<&zbus::names::UniqueName<'_>>,
    ) -> zbus::fdo::Result<Identity> {
        let sender =
            sender.ok_or_else(|| zbus::fdo::Error::Failed("anonymous caller refused".into()))?;
        let dbus = zbus::fdo::DBusProxy::new(conn).await?;
        let bus_name = zbus::names::BusName::from(sender.to_owned());
        let uid = dbus
            .get_connection_unix_user(bus_name.clone())
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("caller uid resolution failed: {e}")))?;
        let pid = dbus
            .get_connection_unix_process_id(bus_name)
            .await
            .unwrap_or(0);
        let pinned_pidfd = pid > 0 && sysffi::pidfd_open(pid).is_some();
        let cgroup = if pid > 0 {
            sysffi::cgroup_of_pid(pid)
        } else {
            None
        };
        Ok(Identity {
            uid,
            pid,
            pinned_pidfd,
            cgroup,
        })
    }

    fn dispatch(&self, ev: Event) -> zbus::fdo::Result<()> {
        self.ev
            .send(ev)
            .map_err(|_| zbus::fdo::Error::Failed("session core gone".into()))
    }

    async fn lifecycle(
        &self,
        conn: &zbus::Connection,
        action: Action,
        sender: Option<&zbus::names::UniqueName<'_>>,
    ) -> zbus::fdo::Result<()> {
        let id = self.identity_of(conn, sender).await?;
        tracing::info!(
            target: "bus",
            action = action.name(),
            uid = id.uid,
            pid = id.pid,
            pidfd = id.pinned_pidfd,
            cgroup = id.cgroup.as_deref().unwrap_or("?"),
            "lifecycle request"
        );
        let (tx, rx) = oneshot::channel();
        self.dispatch(Event::Lifecycle {
            action,
            uid: id.uid,
            reply: tx,
        })?;
        rx.await
            .map_err(|_| zbus::fdo::Error::Failed("core dropped reply".into()))?
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }
}

#[zbus::interface(name = "os.lionos.Session1")]
impl SessionIface {
    // ── methods (spec 02 §4) ────────────────────────────────────────

    async fn logout(
        &self,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        self.lifecycle(conn, Action::Logout, hdr.sender()).await
    }

    async fn restart(
        &self,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        self.lifecycle(conn, Action::Restart, hdr.sender()).await
    }

    async fn shutdown(
        &self,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        self.lifecycle(conn, Action::Shutdown, hdr.sender()).await
    }

    async fn suspend(
        &self,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        self.lifecycle(conn, Action::Suspend, hdr.sender()).await
    }

    async fn hibernate(
        &self,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        self.lifecycle(conn, Action::Hibernate, hdr.sender()).await
    }

    async fn lock(
        &self,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        let id = self.identity_of(conn, hdr.sender()).await?;
        let (tx, rx) = oneshot::channel();
        self.dispatch(Event::Lock {
            uid: id.uid,
            reply: tx,
        })?;
        rx.await
            .map_err(|_| zbus::fdo::Error::Failed("core dropped reply".into()))?
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    async fn switch_user(
        &self,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        let id = self.identity_of(conn, hdr.sender()).await?;
        let (tx, rx) = oneshot::channel();
        self.dispatch(Event::SwitchUser {
            uid: id.uid,
            reply: tx,
        })?;
        rx.await
            .map_err(|_| zbus::fdo::Error::Failed("core dropped reply".into()))?
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    async fn inhibit(
        &self,
        what: String,
        who: String,
        why: String,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<OwnedFd> {
        // Bounds first (spec §8: validate and bound every input).
        if what.len() > 64
            || who.len() > crate::inhibitors::MAX_WHO_LEN
            || why.len() > crate::inhibitors::MAX_WHY_LEN
        {
            return Err(zbus::fdo::Error::InvalidArgs(
                "what/who/why exceed bounds".into(),
            ));
        }
        let id = self.identity_of(conn, hdr.sender()).await?;
        let owner = hdr
            .sender()
            .map(|s| s.to_string())
            .unwrap_or_else(|| ":anon".to_string());
        let (tx, rx) = oneshot::channel();
        self.dispatch(Event::InhibitReq {
            what,
            who,
            why,
            owner,
            uid: id.uid,
            reply: tx,
        })?;
        rx.await
            .map_err(|_| zbus::fdo::Error::Failed("core dropped reply".into()))?
            .map_err(zbus::fdo::Error::Failed)
            .map(|stream| {
                // Ownership of the peer end moves to the caller through
                // the D-Bus message; when the last duplicate in the client
                // closes, the core's EOF watch releases the inhibitor.
                OwnedFd::from(std::os::fd::OwnedFd::from(stream))
            })
    }

    async fn register_client(
        &self,
        app_id: String,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        let id = self.identity_of(conn, hdr.sender()).await?;
        let owner = hdr.sender().map(|s| s.to_string()).unwrap_or_default();
        let (tx, rx) = oneshot::channel();
        self.dispatch(Event::RegisterClient {
            app_id,
            owner,
            uid: id.uid,
            pid: id.pid,
            reply: tx,
        })?;
        rx.await
            .map_err(|_| zbus::fdo::Error::Failed("core dropped reply".into()))?
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    /// Documented extension: acknowledge QueryEndSession without
    /// disconnecting (a disconnect also counts as an ack).
    async fn end_session_reply(
        &self,
        app_id: String,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        let _ = self.identity_of(conn, hdr.sender()).await?;
        let (tx, rx) = oneshot::channel();
        self.dispatch(Event::EndSessionReply { app_id, reply: tx })?;
        rx.await
            .map_err(|_| zbus::fdo::Error::Failed("core dropped reply".into()))?
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    // ── properties (spec 02 §4) ─────────────────────────────────────

    #[zbus(property)]
    async fn state(&self) -> zbus::fdo::Result<String> {
        let mut rx = self.shared.state.clone();
        let v = rx.borrow_and_update().clone();
        Ok(v)
    }

    #[zbus(property)]
    async fn inhibited_actions(&self) -> zbus::fdo::Result<Vec<String>> {
        let mut rx = self.shared.inhibited_actions.clone();
        let v = rx.borrow_and_update().clone();
        Ok(v)
    }

    #[zbus(property)]
    async fn safe_mode(&self) -> zbus::fdo::Result<bool> {
        let mut rx = self.shared.safe_mode.clone();
        let v = *rx.borrow_and_update();
        Ok(v)
    }

    /// Documented extension (spec 02 §3 "show which app is blocking"):
    /// non-acked clients + active inhibitors.
    #[zbus(property)]
    async fn blockers(&self) -> zbus::fdo::Result<Vec<String>> {
        let mut rx = self.shared.blockers.clone();
        let v = rx.borrow_and_update().clone();
        Ok(v)
    }

    // ── signals (spec 02 §4) ────────────────────────────────────────

    #[zbus(signal)]
    async fn session_ready(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn query_end_session(emitter: &SignalEmitter<'_>, flags: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn end_session(emitter: &SignalEmitter<'_>, flags: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn service_failed(
        emitter: &SignalEmitter<'_>,
        name: String,
        reason: String,
    ) -> zbus::Result<()>;
}

/// Watch core property channels → org.freedesktop.DBus.Properties.
/// PropertiesChanged.
async fn property_watcher(conn: Arc<zbus::Connection>, shared: Shared) {
    let emitter = match SignalEmitter::new(conn.as_ref(), PATH) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(target: "bus", "property emitter: {e}");
            return;
        }
    };
    let mut state = shared.state.clone();
    let mut inhibited = shared.inhibited_actions.clone();
    let mut safe_mode = shared.safe_mode.clone();
    let mut blockers = shared.blockers.clone();
    loop {
        tokio::select! {
            r = state.changed() => { if r.is_err() { return; } }
            r = inhibited.changed() => { if r.is_err() { return; } }
            r = safe_mode.changed() => { if r.is_err() { return; } }
            r = blockers.changed() => { if r.is_err() { return; } }
        }
        let v = state.borrow_and_update().clone();
        let i = inhibited.borrow_and_update().clone();
        let s = *safe_mode.borrow_and_update();
        let b = blockers.borrow_and_update().clone();
        let changed: std::collections::HashMap<&str, zbus::zvariant::Value<'_>> = [
            ("State", zbus::zvariant::Value::new(v)),
            ("InhibitedActions", zbus::zvariant::Value::new(i)),
            ("SafeMode", zbus::zvariant::Value::new(s)),
            ("Blockers", zbus::zvariant::Value::new(b)),
        ]
        .into_iter()
        .collect();
        let invalidated: Vec<&str> = vec![];
        let body = (IFACE, changed, invalidated);
        if let Err(e) = emitter
            .emit(
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                &body,
            )
            .await
        {
            tracing::warn!(target: "bus", "properties-changed emit failed: {e}");
        }
    }
}

/// Emit core-requested signals through the generated signals trait
/// (implemented for `SignalEmitter`).
async fn signal_dispatcher(
    conn: Arc<zbus::Connection>,
    mut rx: mpsc::UnboundedReceiver<crate::session::SignalReq>,
) {
    let emitter = match SignalEmitter::new(conn.as_ref(), PATH) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(target: "bus", "signal emitter: {e}");
            return;
        }
    };
    use crate::bus::SessionIfaceSignals as _;
    while let Some(req) = rx.recv().await {
        let r = match req {
            crate::session::SignalReq::SessionReady => emitter.session_ready().await,
            crate::session::SignalReq::QueryEndSession(f) => emitter.query_end_session(f).await,
            crate::session::SignalReq::EndSession(f) => emitter.end_session(f).await,
            crate::session::SignalReq::ServiceFailed(n, reason) => {
                emitter.service_failed(n, reason).await
            }
        };
        if let Err(e) = r {
            tracing::warn!(target: "bus", "signal emit failed: {e}");
        }
    }
}

/// Detect vanished clients (NameOwnerChanged: unique name losing its
/// owner) so the core treats their registrations as acked and sweeps
/// their inhibitors (spec 02 §6 inhibitor leak).
async fn vanishing_watcher(conn: Arc<zbus::Connection>, ev: mpsc::UnboundedSender<Event>) {
    let dbus = match zbus::fdo::DBusProxy::new(conn.as_ref()).await {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(target: "bus", "NameOwnerChanged watch unavailable: {e}");
            return;
        }
    };
    let mut stream = match dbus.receive_name_owner_changed().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(target: "bus", "NameOwnerChanged stream failed: {e}");
            return;
        }
    };
    while let Some(sig) = stream.next().await {
        if let Ok(args) = sig.args() {
            let name = args.name().to_string();
            let gone = args.old_owner().is_some() && args.new_owner().is_none();
            if name.starts_with(':') && gone {
                let _ = ev.send(Event::ClientGone { owner: name });
            }
        }
    }
}

/// Watch systemd JobRemoved for supervised units (systemd mode):
/// "done" → service ready, "failed" → service exit (code 1).
async fn job_watcher(
    conn: Arc<zbus::Connection>,
    ev: mpsc::UnboundedSender<Event>,
) -> zbus::Result<()> {
    let proxy = zbus::Proxy::new(
        conn.as_ref(),
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
    )
    .await?;
    let mut stream = proxy.receive_signal("JobRemoved").await?;
    while let Some(msg) = stream.next().await {
        if let Ok((_id, unit, result)) = msg.body().deserialize::<(u32, String, String)>() {
            let _ = ev.send(Event::ServiceJobDone { unit, result });
        }
    }
    Ok(())
}

/// Serve the interface on a fresh session-bus connection; returns the
/// connection with the well-known name owned.
pub async fn serve(
    cfg: &Config,
    ev: mpsc::UnboundedSender<Event>,
    shared: Shared,
    sig_rx: mpsc::UnboundedReceiver<crate::session::SignalReq>,
    systemd_mode: bool,
) -> crate::Result<Arc<zbus::Connection>> {
    // Interface first, name second: no window where the name resolves
    // but the object is not served yet (methods resolve the connection
    // per-call through the macro's #[zbus(connection)] injection).
    let conn = zbus::connection::Builder::session()
        .map_err(|e| crate::Error::Bus(format!("session bus: {e}")))?
        .serve_at(
            PATH,
            SessionIface {
                ev: ev.clone(),
                shared: shared.clone(),
            },
        )
        .map_err(|e| crate::Error::Bus(format!("serve_at: {e}")))?
        .name(cfg.bus.name.as_str())
        .map_err(|e| crate::Error::Bus(format!("name {}: {e}", cfg.bus.name)))?
        .build()
        .await
        .map_err(|e| crate::Error::Bus(format!("connection: {e}")))?;
    let conn = Arc::new(conn);
    tracing::info!(target: "bus", "owning {} at {PATH}", cfg.bus.name);

    tokio::spawn(property_watcher(conn.clone(), shared));
    tokio::spawn(signal_dispatcher(conn.clone(), sig_rx));
    tokio::spawn(vanishing_watcher(conn.clone(), ev.clone()));
    if systemd_mode {
        let c = conn.clone();
        let e = ev.clone();
        tokio::spawn(async move {
            if let Err(e) = job_watcher(c, e).await {
                tracing::warn!(target: "bus", "JobRemoved watch failed: {e}");
            }
        });
    }
    Ok(conn)
}
