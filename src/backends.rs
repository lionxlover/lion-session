#![forbid(unsafe_code)]
//! Real backends.
//!
//! - [`DirectLauncher`] — spawns session children with the session env +
//!   a per-child abstract `NOTIFY_SOCKET` (children use plain
//!   `sd_notify(0, "READY=1")`; no pid-credential parsing needed because
//!   every child owns a unique socket address).
//! - `ZbusSystemdUser` / `ZbusLogind` — D-Bus via zbus 5 (feature
//!   `real-backends`), with `SystemctlCli` / `LoginctlCli` fallbacks for
//!   the spec 02 §6 rule: "logind unreachable: degrade to direct
//!   systemctl calls and log loudly".
//! - `NotificationsNotifier` — org.freedesktop.Notifications (crash-loop
//!   and safe-mode alerts). Notification failure is never fatal.

use crate::error::{Error, Result};
use crate::ports::{ChildProcess, ExitInfo, Launcher, Spawned};
use async_trait::async_trait;

use std::sync::Mutex;
use std::time::Duration;

// ── Direct child supervision ──────────────────────────────────────────

/// A real child process. `wait()` is called exactly once by the core's
/// wait task; the result is cached for later idempotent reads. `kill`
/// signals by pid — safe (no reaping races: `kill_on_drop` is set, and
/// the supervisor only kills while the wait task is live).
struct DirectChild {
    pid: u32,
    inner: tokio::sync::Mutex<Option<tokio::process::Child>>,
    done: Mutex<Option<ExitInfo>>,
}

#[async_trait]
impl ChildProcess for DirectChild {
    fn pid(&self) -> u32 {
        self.pid
    }

    async fn wait(&self) -> Result<ExitInfo> {
        let mut slot = self.inner.lock().await;
        if let Some(mut child) = slot.take() {
            let status = child
                .wait()
                .await
                .map_err(|e| Error::Service(format!("wait pid {}: {e}", self.pid)))?;
            let info = ExitInfo {
                code: status.code(),
                abnormal: status.code().is_none(),
            };
            *self.done.lock().unwrap() = Some(info.clone());
            Ok(info)
        } else {
            self.done
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| Error::Service("wait already in flight".into()))
        }
    }

    fn kill(&self) {
        // sysffi (audited): pid reuse is bounded by kill_on_drop and the
        // supervisor only firing kills for children it still tracks.
        crate::sysffi::kill_pid(self.pid);
    }
}

/// Sanitize a service name into an abstract-socket path component.
fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "svc".into()
    } else {
        cleaned.chars().take(48).collect()
    }
}

/// Spawns real child processes.
#[derive(Clone, Copy, Default)]
pub struct DirectLauncher;

#[async_trait]
impl Launcher for DirectLauncher {
    async fn spawn(
        &self,
        name: &str,
        argv: &[String],
        env: &[(String, String)],
    ) -> Result<Spawned> {
        if argv.is_empty() {
            return Err(Error::Service(format!("empty argv for {name}")));
        }
        let pid = std::process::id();
        let safe = sanitize(name);
        static SPAWN_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SPAWN_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Unique per spawn: a respawning service would otherwise hit
        // EADDRINUSE on the still-bound previous reader.
        let addr = format!("@lion-session/{pid}/{safe}-{seq}");
        let notify = crate::mocks::bind_abstract(&crate::mocks::abstract_name_of(&addr))
            .map_err(|e| Error::Io("notify socket bind".into(), e))?;

        let mut cmd = tokio::process::Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .env_clear()
            .envs(env.iter().cloned())
            .env("NOTIFY_SOCKET", &addr)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let child = cmd
            .spawn()
            .map_err(|e| Error::Service(format!("spawn {name} ({}): {e}", argv[0])))?;
        let cpid = child.id().unwrap_or(0);
        tracing::info!(target: "spawn", name = name, pid = cpid, "started");
        Ok(Spawned {
            child: Box::new(DirectChild {
                pid: cpid,
                inner: tokio::sync::Mutex::new(Some(child)),
                done: Mutex::new(None),
            }),
            notify: Some(notify),
        })
    }
}

// ── Compositor readiness (Wayland socket fallback) ────────────────────

/// Readiness fallback: the compositor's Wayland socket appearing under
/// the runtime dir. Polling only runs during startup (bounded by
/// `timeout`); the primary readiness path is the READY=1 notify frame.
#[derive(Clone)]
pub struct WaylandSocketWatch {
    pub runtime_dir: String,
    pub display: String,
}

#[async_trait]
impl crate::ports::ReadyWatch for WaylandSocketWatch {
    async fn wait_ready(&self, timeout: Duration) -> Result<()> {
        let path = format!("{}/{}", self.runtime_dir, self.display);
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if std::path::Path::new(&path).exists() {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(Error::Service(format!(
                    "wayland socket {path} did not appear"
                )));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

// ── CLI fallbacks (always compiled) ───────────────────────────────────

async fn run_cli(argv: &[&str]) -> Result<std::process::Output> {
    let out = tokio::process::Command::new(argv[0])
        .args(&argv[1..])
        .output()
        .await
        .map_err(|e| Error::Systemd(format!("{}: {e}", argv[0])))?;
    if !out.status.success() {
        return Err(Error::Systemd(format!(
            "{}: {}",
            argv[0],
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out)
}

/// `systemctl --user` fallback when the user manager is not on the bus.
#[derive(Clone, Copy, Default)]
pub struct SystemctlCliUser;

#[async_trait]
impl crate::ports::SystemdUser for SystemctlCliUser {
    async fn start_unit(&self, name: &str, mode: &str) -> Result<()> {
        if mode == "fail" {
            run_cli(&["systemctl", "--user", "start", "--no-block", "--fail", name]).await?;
        } else {
            run_cli(&["systemctl", "--user", "start", "--no-block", name]).await?;
        }
        Ok(())
    }
    async fn stop_unit(&self, name: &str, _mode: &str) -> Result<()> {
        run_cli(&["systemctl", "--user", "stop", "--no-block", name]).await?;
        Ok(())
    }
    async fn set_environment(&self, assignments: &[String]) -> Result<()> {
        if assignments.is_empty() {
            return Ok(());
        }
        let mut argv = vec!["systemctl", "--user", "set-environment"];
        let owned: Vec<&str> = assignments.iter().map(|s| s.as_str()).collect();
        argv.extend(owned);
        let ref_argv: Vec<&str> = argv;
        run_cli(&ref_argv).await?;
        Ok(())
    }
}

/// `systemctl` / `loginctl` fallback when logind is unreachable (spec
/// 02 §6: degrade to direct calls and log loudly).
#[derive(Clone, Copy, Default)]
pub struct LoginctlCli;

#[async_trait]
impl crate::ports::Logind for LoginctlCli {
    async fn power_off(&self) -> Result<()> {
        tracing::warn!(target: "logind", "logind unreachable — using systemctl poweroff");
        run_cli(&["systemctl", "poweroff"]).await?;
        Ok(())
    }
    async fn reboot(&self) -> Result<()> {
        tracing::warn!(target: "logind", "logind unreachable — using systemctl reboot");
        run_cli(&["systemctl", "reboot"]).await?;
        Ok(())
    }
    async fn suspend(&self) -> Result<()> {
        tracing::warn!(target: "logind", "logind unreachable — using systemctl suspend");
        run_cli(&["systemctl", "suspend"]).await?;
        Ok(())
    }
    async fn hibernate(&self) -> Result<()> {
        tracing::warn!(target: "logind", "logind unreachable — using systemctl hibernate");
        run_cli(&["systemctl", "hibernate"]).await?;
        Ok(())
    }
    async fn lock(&self, session_id: Option<&str>) -> Result<()> {
        match session_id {
            Some(id) => run_cli(&["loginctl", "lock-session", id]).await.map(|_| ()),
            None => run_cli(&["loginctl", "lock-sessions"]).await.map(|_| ()),
        }
    }
    async fn activate_session(&self, id: &str) -> Result<()> {
        run_cli(&["loginctl", "activate", id]).await.map(|_| ())
    }
}

// ── zbus backends (feature real-backends) ─────────────────────────────

#[cfg(feature = "real-backends")]
mod zbus_backends {
    use super::*;
    use crate::error::{Error, Result};

    fn map_err(ctx: &'static str, e: impl std::fmt::Display) -> Error {
        Error::Bus(format!("{ctx}: {e}"))
    }

    async fn manager_proxy(conn: &zbus::Connection) -> Result<zbus::Proxy<'static>> {
        zbus::Proxy::new_owned(
            conn.clone(),
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
        )
        .await
        .map_err(|e| map_err("systemd proxy", e))
    }

    async fn logind_proxy(conn: &zbus::Connection) -> Result<zbus::Proxy<'static>> {
        zbus::Proxy::new_owned(
            conn.clone(),
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
        )
        .await
        .map_err(|e| map_err("logind proxy", e))
    }

    /// org.freedesktop.systemd1 user-manager client.
    #[derive(Clone)]
    pub struct ZbusSystemdUser {
        conn: zbus::Connection,
    }

    impl ZbusSystemdUser {
        pub async fn connect() -> Result<Self> {
            let conn = zbus::connection::Builder::session()
                .map_err(|e| map_err("session bus", e))?
                .build()
                .await
                .map_err(|e| map_err("session bus", e))?;
            Ok(ZbusSystemdUser { conn })
        }
    }

    #[async_trait]
    impl crate::ports::SystemdUser for ZbusSystemdUser {
        async fn start_unit(&self, name: &str, mode: &str) -> Result<()> {
            manager_proxy(&self.conn)
                .await?
                .call_method("StartUnit", &(name, mode))
                .await
                .map_err(|e| map_err("StartUnit", e))?;
            Ok(())
        }
        async fn stop_unit(&self, name: &str, mode: &str) -> Result<()> {
            manager_proxy(&self.conn)
                .await?
                .call_method("StopUnit", &(name, mode))
                .await
                .map_err(|e| map_err("StopUnit", e))?;
            Ok(())
        }
        async fn set_environment(&self, assignments: &[String]) -> Result<()> {
            let v: Vec<&str> = assignments.iter().map(|s| s.as_str()).collect();
            manager_proxy(&self.conn)
                .await?
                .call_method("SetEnvironment", &(v))
                .await
                .map_err(|e| map_err("SetEnvironment", e))?;
            Ok(())
        }
    }

    /// org.freedesktop.login1 client. Power methods are called with the
    /// historical `(interactive: bool)` signature and retried without
    /// arguments when the daemon rejects the args (systemd ≥ 252 variant).
    #[derive(Clone)]
    pub struct ZbusLogind {
        conn: zbus::Connection,
    }

    impl ZbusLogind {
        pub async fn connect() -> Result<Self> {
            let conn = zbus::connection::Builder::session()
                .map_err(|e| map_err("session bus", e))?
                .build()
                .await
                .map_err(|e| map_err("session bus", e))?;
            Ok(ZbusLogind { conn })
        }

        async fn power(&self, method: &str) -> Result<()> {
            let p = logind_proxy(&self.conn).await?;
            // Historical signature takes (interactive: bool); newer
            // daemons dropped the argument — try both, fail on both.
            match p.call_method(method, &(false)).await {
                Ok(_) => Ok(()),
                Err(_) => p
                    .call_method(method, &())
                    .await
                    .map(|_| ())
                    .map_err(|e| Error::Bus(format!("{method}: {e}"))),
            }
        }
    }

    #[async_trait]
    impl crate::ports::Logind for ZbusLogind {
        async fn power_off(&self) -> Result<()> {
            self.power("PowerOff").await
        }
        async fn reboot(&self) -> Result<()> {
            self.power("Reboot").await
        }
        async fn suspend(&self) -> Result<()> {
            self.power("Suspend").await
        }
        async fn hibernate(&self) -> Result<()> {
            self.power("Hibernate").await
        }
        async fn lock(&self, session_id: Option<&str>) -> Result<()> {
            let p = logind_proxy(&self.conn).await?;
            match session_id {
                Some(id) => p
                    .call_method("LockSession", &(id))
                    .await
                    .map(|_| ())
                    .map_err(|e| map_err("LockSession", e)),
                None => p
                    .call_method("LockSessions", &())
                    .await
                    .map(|_| ())
                    .map_err(|e| map_err("LockSessions", e)),
            }
        }
        async fn activate_session(&self, id: &str) -> Result<()> {
            logind_proxy(&self.conn)
                .await?
                .call_method("ActivateSession", &(id))
                .await
                .map(|_| ())
                .map_err(|e| map_err("ActivateSession", e))
        }
    }

    /// lion-auth client (os.lionos.Auth1.Authorize) — fail closed: any
    /// transport error means deny (spec 02 §8).
    pub struct LionAuthClient {
        conn: zbus::Connection,
        bus_name: String,
        timeout: std::time::Duration,
    }

    impl LionAuthClient {
        pub async fn connect(bus_name: &str, timeout: Duration) -> Result<Self> {
            let conn = zbus::connection::Builder::session()
                .map_err(|e| map_err("session bus", e))?
                .build()
                .await
                .map_err(|e| map_err("session bus", e))?;
            Ok(LionAuthClient {
                conn,
                bus_name: bus_name.to_string(),
                timeout,
            })
        }
    }

    #[async_trait]
    impl crate::ports::Authorizer for LionAuthClient {
        async fn authorize(&self, action: &str, uid: u32) -> Result<bool> {
            let dest = zbus::names::BusName::try_from(self.bus_name.clone())
                .map_err(|e| map_err("auth bus name", e))?;
            let p = zbus::Proxy::new_owned(
                self.conn.clone(),
                dest,
                "/os/lionos/Auth",
                "os.lionos.Auth1",
            )
            .await
            .map_err(|e| map_err("auth proxy", e))?;
            let args = (action, uid);
            let call = p.call_method("Authorize", &args);
            let raw = tokio::time::timeout(self.timeout, call).await;
            match raw {
                Ok(Ok(msg)) => msg
                    .body()
                    .deserialize::<bool>()
                    .map_err(|e| map_err("auth decode", e)),
                // Fail closed: unreachable / slow lion-auth denies.
                Ok(Err(e)) => {
                    tracing::warn!(target: "authz", "lion-auth error — denying: {e}");
                    Ok(false)
                }
                Err(_) => {
                    tracing::warn!(target: "authz", "lion-auth timeout — denying");
                    Ok(false)
                }
            }
        }
    }

    /// org.freedesktop.Notifications client; failures degrade to logs.
    #[derive(Clone)]
    pub struct NotificationsNotifier {
        conn: zbus::Connection,
    }

    impl NotificationsNotifier {
        pub async fn connect() -> Result<Self> {
            let conn = zbus::connection::Builder::session()
                .map_err(|e| map_err("session bus", e))?
                .build()
                .await
                .map_err(|e| map_err("session bus", e))?;
            Ok(NotificationsNotifier { conn })
        }
    }

    #[async_trait]
    impl crate::ports::Notifier for NotificationsNotifier {
        async fn notify(&self, summary: &str, body: &str) -> Result<()> {
            let p = zbus::Proxy::new_owned(
                self.conn.clone(),
                "org.freedesktop.Notifications",
                "/org/freedesktop/Notifications",
                "org.freedesktop.Notifications",
            )
            .await
            .map_err(|e| map_err("notifications proxy", e))?;
            let actions: Vec<&str> = vec![];
            let hints: std::collections::HashMap<&str, zbus::zvariant::Value> =
                std::collections::HashMap::new();
            let res: Result<u32> = p
                .call_method(
                    "Notify",
                    &(
                        "lion-session",
                        0u32,
                        "",
                        summary,
                        body,
                        actions,
                        hints,
                        -1i32,
                    ),
                )
                .await
                .map_err(|e| map_err("Notify", e))?
                .body()
                .deserialize::<u32>()
                .map_err(|e| map_err("Notify decode", e));
            match res {
                Ok(_) => Ok(()),
                Err(e) => {
                    tracing::warn!(
                        target: "notify",
                        "notification failed ({e}); logging instead: {summary}: {body}"
                    );
                    Ok(())
                }
            }
        }
    }
}

#[cfg(feature = "real-backends")]
pub use zbus_backends::{LionAuthClient, NotificationsNotifier, ZbusLogind, ZbusSystemdUser};

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn direct_launcher_spawns_and_reaps() {
        let l = DirectLauncher;
        let s = l
            .spawn(
                "true-test",
                &["/bin/true".to_string()],
                &[("HOME".into(), "/tmp".into())],
            )
            .await
            .unwrap();
        assert!(s.notify.is_some());
        let info = s.child.wait().await.unwrap();
        assert_eq!(info.code, Some(0));
        assert!(info.is_ok());
    }

    #[tokio::test]
    async fn direct_launcher_exit_code_propagates() {
        let l = DirectLauncher;
        let s = l
            .spawn("false-test", &["/bin/false".to_string()], &[])
            .await
            .unwrap();
        let info = s.child.wait().await.unwrap();
        assert_eq!(info.code, Some(1));
        assert!(!info.is_ok());
    }

    #[tokio::test]
    async fn direct_launcher_kills_runners() {
        let l = DirectLauncher;
        let s = l
            .spawn("sleeper", &["/bin/sleep".to_string(), "300".into()], &[])
            .await
            .unwrap();
        let pid = s.child.pid();
        assert!(pid > 1);
        s.child.kill();
        let info = tokio::time::timeout(Duration::from_secs(5), s.child.wait())
            .await
            .expect("killed child exits promptly")
            .unwrap();
        assert!(info.abnormal);
    }

    #[tokio::test]
    async fn direct_launcher_rejects_empty_argv() {
        let l = DirectLauncher;
        let e = l.spawn("empty", &[], &[]).await.unwrap_err();
        assert!(matches!(e, Error::Service(_)));
    }

    #[tokio::test]
    async fn direct_launcher_missing_binary_is_service_error() {
        let l = DirectLauncher;
        let e = l
            .spawn("ghost", &["/nonexistent/binary".to_string()], &[])
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Service(_)));
    }

    #[tokio::test]
    async fn child_notify_socket_receives_frames() {
        // A real child sending sd_notify frames to its NOTIFY_SOCKET —
        // the exact readiness path the core uses.
        let l = DirectLauncher;
        let s = l
            .spawn(
                "notifier",
                &[
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "python3 -c 'import socket,os; s=socket.socket(socket.AF_UNIX,socket.SOCK_DGRAM); s.connect(os.environ[\"NOTIFY_SOCKET\"].replace(\"@\", chr(0), 1)); s.send(b\"READY=1\")'".to_string(),
                ],
                &[],
            )
            .await
            .unwrap();
        let sock = s.notify.unwrap();
        let mut buf = [0u8; 64];
        let (n, _) = tokio::time::timeout(Duration::from_secs(5), sock.recv_from(&mut buf))
            .await
            .expect("notify frame arrives")
            .unwrap();
        assert_eq!(&buf[..n], b"READY=1");
        // reap
        let _ = s.child.wait().await;
    }

    #[tokio::test]
    async fn sanitize_names() {
        assert_eq!(sanitize("panel"), "panel");
        assert_eq!(sanitize("weird/../name"), "weird____name");
        assert_eq!(sanitize("!!"), "__");
        assert_eq!(sanitize(""), "svc");
        assert_eq!(sanitize(&"x".repeat(100)).len(), 48);
    }
}
