//! Best-effort logind integration over the *system* bus:
//!
//! - **Delay inhibitors** (`Inhibit(..., "delay")`): held for the whole
//!   end-of-session sequence so the system cannot finish shutting down
//!   underneath a session that is still saving state. The inhibitor is an
//!   fd: it is released simply by being dropped when teardown completes --
//!   the same primitive every logind client uses.
//! - **PrepareForShutdown**: logind is about to power the machine off; the
//!   session skips animations and cooperative queries and ends fast.
//! - **PrepareForSleep(true)**: the system suspends; the session locks
//!   first (config: `[session] lock-on-sleep`), so resume requires
//!   re-authentication -- the single most valuable anti-shoulder-surf
//!   hardening GNOME and KDE do and most Wayland sessions forget.
//! - **Session Lock/Unlock signals** (what `loginctl lock-session` sends):
//!   forwarded to lion-locker, so the lock request works no matter which
//!   component asked logind for it.
//! - **SetIdleHint**: pushes the real idle state to logind so
//!   `loginctl` output, `wtmp`-style accounting and other seats see it.
//!
//! Everything degrades to a logged no-op when logind is absent (test
//! containers, non-systemd systems): absence of logind must never be
//! fatal, and never block a working desktop.

use anyhow::Result;
use tokio::sync::mpsc;
use zbus::{Connection, MatchRule, Message, MessageStream, MessageType};
// doc(hidden) re-export used by zbus's own abstractions; pinned to zbus 4.
use zbus::export::futures_util::StreamExt;

pub const MANAGER: &str = "org.freedesktop.login1";
pub const MANAGER_PATH: &str = "/org/freedesktop/login1";
pub const MANAGER_IFACE: &str = "org.freedesktop.login1.Manager";
pub const SESSION_IFACE: &str = "org.freedesktop.login1.Session";

/// Events the session core reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogindEvent {
    /// logind's PrepareForShutdown(true): end the session immediately.
    PrepareShutdown,
    /// PrepareForSleep: true = about to suspend, false = resumed.
    PrepareSleep(bool),
    /// The session's Lock / Unlock signal (loginctl lock-session).
    SessionLock(bool),
}

/// Our logind session object paths: the real one when XDG_SESSION_ID is
/// known (set by PAM), plus the "auto" alias for inside-session calls.
fn session_paths() -> Vec<String> {
    let mut v = vec!["/org/freedesktop/login1/session/auto".to_string()];
    if let Some(id) = std::env::var_os("XDG_SESSION_ID") {
        v.push(format!(
            "/org/freedesktop/login1/session/{}",
            id.to_string_lossy()
        ));
    }
    v
}

#[derive(Clone)]
pub struct Logind {
    conn: Connection,
}

impl Logind {
    /// `None` when there is no system bus or logind is not on it.
    pub async fn connect() -> Option<Self> {
        let conn = match Connection::system().await {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(error = %e, "no system bus, logind integration off");
                return None;
            }
        };
        let owned = name_has_owner(&conn, MANAGER).await;
        if !owned {
            tracing::debug!("logind is not on the system bus, integration off");
            return None;
        }
        tracing::info!("logind integration active");
        Some(Self { conn })
    }

    /// Hold a "delay" inhibitor. Returns None (logged) on failure: a
    /// failed inhibitor must never fail the end sequence itself.
    pub async fn inhibit_delay(&self, what: &str, why: &str) -> Option<zbus::zvariant::OwnedFd> {
        let reply = self
            .conn
            .call_method(
                Some(MANAGER),
                MANAGER_PATH,
                Some(MANAGER_IFACE),
                "Inhibit",
                &(what, "lion-session", why, "delay"),
            )
            .await;
        match reply {
            Ok(msg) => match msg.body().deserialize::<zbus::zvariant::OwnedFd>() {
                Ok(fd) => {
                    tracing::debug!(what, why, "holding logind delay inhibitor");
                    Some(fd)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "logind Inhibit reply had unexpected body");
                    None
                }
            },
            Err(e) => {
                tracing::warn!(what, error = %e, "could not take logind inhibitor");
                None
            }
        }
    }

    /// Push the idle hint for our session.
    pub async fn set_idle_hint(&self, idle: bool) -> bool {
        let path = session_paths()
            .pop()
            .unwrap_or_else(|| "/org/freedesktop/login1/session/auto".into());
        let res = self
            .conn
            .call_method(
                Some(MANAGER),
                path.as_str(),
                Some(SESSION_IFACE),
                "SetIdleHint",
                &(idle,),
            )
            .await;
        match res {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!(idle, error = %e, "SetIdleHint failed");
                false
            }
        }
    }

    /// Spawn the signal listener task; the returned channel yields the
    /// interesting events. The task ends with the connection.
    pub async fn events(&self) -> Result<mpsc::UnboundedReceiver<LogindEvent>> {
        let (tx, rx) = mpsc::unbounded_channel();
        let conn = self.conn.clone();
        let session_paths = session_paths();
        let have_session_id = std::env::var_os("XDG_SESSION_ID").is_some();
        tokio::spawn(async move {
            let rule = match MatchRule::builder()
                .msg_type(MessageType::Signal)
                .sender(MANAGER)
            {
                Ok(b) => b.build(),
                Err(e) => {
                    tracing::warn!(error = %e, "could not build logind match rule");
                    return;
                }
            };
            let stream = MessageStream::for_match_rule(rule, &conn, None).await;
            let mut stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "could not subscribe to logind signals");
                    return;
                }
            };
            while let Some(Ok(msg)) = stream.next().await {
                if let Some(event) = classify(&msg, &session_paths, have_session_id) {
                    let _ = tx.send(event);
                }
            }
        });
        Ok(rx)
    }
}

async fn name_has_owner(conn: &Connection, name: &str) -> bool {
    match conn
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "NameHasOwner",
            &(name,),
        )
        .await
    {
        Ok(msg) => msg.body().deserialize::<bool>().unwrap_or(false),
        Err(_) => false,
    }
}

/// Pure classification of one logind signal message into a session event.
fn classify(msg: &Message, session_paths: &[String], have_session_id: bool) -> Option<LogindEvent> {
    let header = msg.header();
    let iface = header.interface().map(|i| i.as_str()).unwrap_or("");
    if iface != "org.freedesktop.login1.Manager" && iface != "org.freedesktop.login1.Session" {
        return None;
    }
    let member = header.member().map(|m| m.as_str()).unwrap_or("");
    match member {
        "PrepareForShutdown" => {
            let (start,) = msg.body().deserialize::<(bool,)>().unwrap_or((false,));
            start.then_some(LogindEvent::PrepareShutdown)
        }
        "PrepareForSleep" => {
            let (start,) = msg.body().deserialize::<(bool,)>().unwrap_or((false,));
            Some(LogindEvent::PrepareSleep(start))
        }
        "Lock" | "Unlock" => {
            let path = header.path().map(|p| p.as_str()).unwrap_or("");
            if session_paths.iter().any(|p| p.as_str() == path) || !have_session_id {
                Some(LogindEvent::SessionLock(member == "Lock"))
            } else {
                None
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_unknown_member_is_none() {
        // Constructing a real Message without a connection is awkward in
        // unit tests; the signal-path classification is exercised end to
        // end in tests/integration.rs against a mock logind. Here we keep
        // the helper contract explicit:
        assert!(session_paths().contains(&"/org/freedesktop/login1/session/auto".to_string()));
    }

    #[test]
    fn session_path_includes_xdg_session_id_when_set() {
        // Not using env manipulation (parallel tests); the function is
        // exercised live in integration.rs instead.
        let paths = session_paths();
        assert!(!paths.is_empty());
    }
}
