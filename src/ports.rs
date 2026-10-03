#![forbid(unsafe_code)]
//! Port seams (hexagonal architecture, mirroring lion-greeter): the core
//! orchestration in `session.rs` talks only to these traits; real D-Bus
//! backends live in `systemd.rs` / `logind.rs` / `authz.rs`, fakes in
//! `mocks.rs`. Async via `async_trait` so dependencies stay `Arc<dyn …>`.
//!
//! One deliberate asymmetry with lion-greeter: processes are supervised
//! through [`ChildProcess`] handles returned by [`Launcher`] instead of a
//! parked-child FFI dance — session children are *unprivileged user
//! processes* (no uid/gid games), so plain `tokio::process` is both safe
//! and idiomatic.

use crate::error::Result;
use async_trait::async_trait;
use std::time::Duration;

/// Exit information of a supervised child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitInfo {
    /// Exit code, when the process exited normally.
    pub code: Option<i32>,
    /// Killed by a signal / otherwise abnormal termination.
    pub abnormal: bool,
}

impl ExitInfo {
    /// "Ran to completion" for restart-policy purposes: exit code 0.
    pub fn is_ok(&self) -> bool {
        self.code == Some(0) && !self.abnormal
    }
}

/// A running supervised process.
#[async_trait]
pub trait ChildProcess: Send + Sync {
    fn pid(&self) -> u32;
    /// Wait for exit (idempotent: repeated waits return the same result).
    async fn wait(&self) -> Result<ExitInfo>;
    /// Best-effort SIGKILL.
    fn kill(&self);
}

/// A freshly spawned session child together with its readiness channel.
pub struct Spawned {
    pub child: Box<dyn ChildProcess>,
    /// The bound abstract-namespace datagram socket the child was told to
    /// `sd_notify("READY=1")` at (`NOTIFY_SOCKET` was exported into its
    /// environment). The core awaits the first READY frame on it.
    pub notify: Option<tokio::net::UnixDatagram>,
}

/// Spawns session children (compositor, shell services, autostart apps)
/// with the session environment. The real implementation is
/// `backends::DirectLauncher`; tests spawn script fakes through the same
/// path (real processes, real exits) or use the scripted
/// `mocks::MockLauncher`.
#[async_trait]
pub trait Launcher: Send + Sync {
    async fn spawn(&self, name: &str, argv: &[String], env: &[(String, String)])
        -> Result<Spawned>;
}

/// org.freedesktop.systemd1 (user manager) — systemd mode.
#[async_trait]
pub trait SystemdUser: Send + Sync {
    /// StartUnit(name, mode). Mode: "fail" (default) or "replace".
    async fn start_unit(&self, name: &str, mode: &str) -> Result<()>;
    async fn stop_unit(&self, name: &str, mode: &str) -> Result<()>;
    /// SetEnvironment(["KEY=VALUE", …]) — the import-environment call.
    async fn set_environment(&self, assignments: &[String]) -> Result<()>;
}

/// org.freedesktop.login1 — power/session actions (spec 02 §3/§4).
#[async_trait]
pub trait Logind: Send + Sync {
    async fn power_off(&self) -> Result<()>;
    async fn reboot(&self) -> Result<()>;
    async fn suspend(&self) -> Result<()>;
    async fn hibernate(&self) -> Result<()>;
    /// LockSession(id) when an id is given, else LockSessions().
    async fn lock(&self, session_id: Option<&str>) -> Result<()>;
    /// ActivateSession(id) — switch-user target.
    async fn activate_session(&self, id: &str) -> Result<()>;
}

/// Authorization seam (spec 02 §8: authorize through lion-auth; identify
/// callers by pidfd/cgroup, never by caller-supplied strings — callers
/// are identified by the bus daemon before reaching this trait).
#[async_trait]
pub trait Authorizer: Send + Sync {
    /// `action` is a stable action id ("session.logout", "power.shutdown",
    /// …); `uid` is the bus-verified caller uid.
    async fn authorize(&self, action: &str, uid: u32) -> Result<bool>;
}

/// User-visible notifications (crash-loop alerts, safe-mode explanation)
/// routed to lion-notifications (org.freedesktop.Notifications).
#[async_trait]
pub trait Notifier: Send + Sync {
    async fn notify(&self, summary: &str, body: &str) -> Result<()>;
}

/// Compositor readiness: primary = READY=1 on the child notify socket;
/// fallback = the Wayland socket path appearing. Wrapped behind a port so
/// tests inject instant readiness.
#[async_trait]
pub trait ReadyWatch: Send + Sync {
    async fn wait_ready(&self, timeout: Duration) -> Result<()>;
}

impl std::fmt::Debug for Spawned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Spawned")
            .field("has_notify", &self.notify.is_some())
            .finish()
    }
}
