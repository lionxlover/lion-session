#![forbid(unsafe_code)]
//! Fakes for every port (integration tests + `--mock` demo mode).
//! Deterministic, scriptable, zero processes where processes would make
//! tests flaky; the launcher fake still binds real notify sockets so the
//! core's readiness path is exercised verbatim.

use crate::error::{Error, Result};
use crate::ports::{ChildProcess, ExitInfo, Launcher, Spawned};
use async_trait::async_trait;
use std::os::linux::net::SocketAddrExt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::Duration;

// ── SystemdUser ───────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct SystemdCalls {
    started: Vec<(String, String)>,
    stopped: Vec<(String, String)>,
    environment: Vec<Vec<String>>,
}

/// Recording fake for org.freedesktop.systemd1.
#[derive(Clone, Default)]
pub struct MockSystemdUser {
    calls: std::sync::Arc<Mutex<SystemdCalls>>,
}

impl MockSystemdUser {
    pub fn started(&self) -> Vec<(String, String)> {
        self.calls.lock().unwrap().started.clone()
    }
    pub fn environment_imports(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().environment.clone()
    }
}

#[async_trait]
impl crate::ports::SystemdUser for MockSystemdUser {
    async fn start_unit(&self, name: &str, mode: &str) -> Result<()> {
        self.calls
            .lock()
            .unwrap()
            .started
            .push((name.to_string(), mode.to_string()));
        Ok(())
    }
    async fn stop_unit(&self, name: &str, mode: &str) -> Result<()> {
        self.calls
            .lock()
            .unwrap()
            .stopped
            .push((name.to_string(), mode.to_string()));
        Ok(())
    }
    async fn set_environment(&self, assignments: &[String]) -> Result<()> {
        self.calls
            .lock()
            .unwrap()
            .environment
            .push(assignments.to_vec());
        Ok(())
    }
}

// ── Logind ────────────────────────────────────────────────────────────

/// Scripted fake for org.freedesktop.login1.
#[derive(Clone, Default)]
pub struct MockLogind {
    pub calls: std::sync::Arc<Mutex<Vec<String>>>,
    pub fail: bool,
}

impl MockLogind {
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl crate::ports::Logind for MockLogind {
    async fn power_off(&self) -> Result<()> {
        self.record("power-off")
    }
    async fn reboot(&self) -> Result<()> {
        self.record("reboot")
    }
    async fn suspend(&self) -> Result<()> {
        self.record("suspend")
    }
    async fn hibernate(&self) -> Result<()> {
        self.record("hibernate")
    }
    async fn lock(&self, session_id: Option<&str>) -> Result<()> {
        self.record(&format!("lock:{}", session_id.unwrap_or("*")))
    }
    async fn activate_session(&self, id: &str) -> Result<()> {
        self.record(&format!("activate:{id}"))
    }
}

impl MockLogind {
    fn record(&self, what: &str) -> Result<()> {
        if self.fail {
            return Err(Error::Logind("logind unreachable (mock)".into()));
        }
        self.calls.lock().unwrap().push(what.to_string());
        Ok(())
    }
}

// ── Authorizer ────────────────────────────────────────────────────────

/// Scripted fake: allow-all, deny-all, or a uid allowlist.
#[derive(Clone)]
pub struct MockAuthorizer {
    pub allow_all: bool,
    pub allowed_uids: Vec<u32>,
    pub asked: std::sync::Arc<Mutex<Vec<(String, u32)>>>,
}

impl Default for MockAuthorizer {
    fn default() -> Self {
        MockAuthorizer {
            allow_all: true,
            allowed_uids: vec![],
            asked: std::sync::Arc::new(Mutex::new(vec![])),
        }
    }
}

#[async_trait]
impl crate::ports::Authorizer for MockAuthorizer {
    async fn authorize(&self, action: &str, uid: u32) -> Result<bool> {
        self.asked.lock().unwrap().push((action.to_string(), uid));
        Ok(self.allow_all || self.allowed_uids.contains(&uid) || uid == 0)
    }
}

// ── Notifier ──────────────────────────────────────────────────────────

/// Recording fake for notifications.
#[derive(Clone, Default)]
pub struct MockNotifier {
    pub sent: std::sync::Arc<Mutex<Vec<(String, String)>>>,
}

#[async_trait]
impl crate::ports::Notifier for MockNotifier {
    async fn notify(&self, summary: &str, body: &str) -> Result<()> {
        self.sent
            .lock()
            .unwrap()
            .push((summary.to_string(), body.to_string()));
        Ok(())
    }
}

// ── ReadyWatch ────────────────────────────────────────────────────────

/// Instantly-ready (or scripted-delay) compositor readiness.
#[derive(Clone, Default)]
pub struct MockReadyWatch {
    pub delay: Duration,
    pub fail: bool,
}

#[async_trait]
impl crate::ports::ReadyWatch for MockReadyWatch {
    async fn wait_ready(&self, _timeout: Duration) -> Result<()> {
        if self.fail {
            return Err(Error::Service(
                "compositor never became ready (mock)".into(),
            ));
        }
        tokio::time::sleep(self.delay).await;
        Ok(())
    }
}

// ── Launcher ──────────────────────────────────────────────────────────

static MOCK_PID: AtomicU32 = AtomicU32::new(900_000);

/// One scripted lifecycle for a fake child.
#[derive(Debug, Clone)]
pub enum Script {
    /// Run "forever" (until killed).
    Run,
    /// Exit after the delay with this status.
    ExitAfter {
        delay: Duration,
        code: i32,
        abnormal: bool,
    },
}

/// A scripted child: no process, but a real pid and a wait future.
pub struct MockChild {
    pid: u32,
    script: Script,
    state: Mutex<crate::mocks::ChildPhase>,
}

enum ChildPhase {
    Running,
    Done(ExitInfo),
}

#[async_trait]
impl ChildProcess for MockChild {
    fn pid(&self) -> u32 {
        self.pid
    }
    async fn wait(&self) -> Result<ExitInfo> {
        // Fast path: already exited.
        {
            let st = self.state.lock().unwrap();
            if let ChildPhase::Done(info) = &*st {
                return Ok(info.clone());
            }
        }
        match self.script.clone() {
            Script::Run => {
                // Park until killed: the kill flips the phase; the poll
                // sleep auto-advances under tokio paused time in tests.
                loop {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    let st = self.state.lock().unwrap();
                    if let ChildPhase::Done(info) = &*st {
                        return Ok(info.clone());
                    }
                }
            }
            Script::ExitAfter {
                delay,
                code,
                abnormal,
            } => {
                let info = ExitInfo {
                    code: Some(code),
                    abnormal,
                };
                {
                    let mut st = self.state.lock().unwrap();
                    *st = ChildPhase::Done(info.clone());
                }
                tokio::time::sleep(delay).await;
                Ok(info)
            }
        }
    }
    fn kill(&self) {
        let mut st = self.state.lock().unwrap();
        if let ChildPhase::Running = *st {
            *st = ChildPhase::Done(ExitInfo {
                code: None,
                abnormal: true,
            });
        }
    }
}

/// Scripted launcher: binds a *real* abstract notify socket per child so
/// the core's readiness path is the production one; tests trigger it by
/// sending `READY=1` to the address (see `notify_addr_of`).
#[derive(Clone, Default)]
pub struct MockLauncher {
    /// Per-name script; unnamed → Run forever.
    pub scripts: std::sync::Arc<std::collections::BTreeMap<String, Script>>,
    /// Fail spawns of these names (spawn error path).
    pub fail_names: Vec<String>,
    pub spawned: std::sync::Arc<Mutex<Vec<String>>>,
    /// (service name → notify socket address) after spawning.
    pub notify_addr: std::sync::Arc<Mutex<std::collections::BTreeMap<String, String>>>,
}

impl MockLauncher {
    pub fn with_scripts(scripts: Vec<(String, Script)>) -> MockLauncher {
        MockLauncher {
            scripts: std::sync::Arc::new(scripts.into_iter().collect()),
            ..Default::default()
        }
    }

    /// Send READY=1 to a spawned child's notify socket (test driver).
    pub async fn make_ready(&self, name: &str) -> std::io::Result<()> {
        let addr = self
            .notify_addr
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no such child"))?;
        send_notify(&addr, b"READY=1").await
    }
}

/// The abstract socket name of an `@`-marked NOTIFY_SOCKET address. The
/// `@` is the sd_notify convention marker and is NOT part of the name —
/// `from_abstract_name` expects the name without a leading NUL (passing
/// one would embed a second NUL and never match real sd_notify clients).
pub fn abstract_name_of(addr: &str) -> Vec<u8> {
    match addr.strip_prefix('@') {
        Some(name) => name.as_bytes().to_vec(),
        None => addr.as_bytes().to_vec(),
    }
}

/// Bind an abstract-namespace datagram socket and hand it to tokio
/// (std supports abstract addresses; tokio's path-based API does not).
/// `name` is the abstract name WITHOUT the leading NUL.
pub fn bind_abstract(name: &[u8]) -> std::io::Result<tokio::net::UnixDatagram> {
    let target = std::os::unix::net::SocketAddr::from_abstract_name(name)?;
    let sock = std::os::unix::net::UnixDatagram::bind_addr(&target)?;
    sock.set_nonblocking(true)?;
    tokio::net::UnixDatagram::from_std(sock)
}

/// Send one datagram to an abstract notify address (sync: test driver).
pub async fn send_notify(addr: &str, payload: &[u8]) -> std::io::Result<()> {
    let name = abstract_name_of(addr);
    let sock = std::os::unix::net::UnixDatagram::unbound()?;
    let target = std::os::unix::net::SocketAddr::from_abstract_name(name)?;
    sock.send_to_addr(payload, &target)?;
    Ok(())
}

#[async_trait]
impl Launcher for MockLauncher {
    async fn spawn(
        &self,
        name: &str,
        argv: &[String],
        _env: &[(String, String)],
    ) -> Result<Spawned> {
        if self.fail_names.iter().any(|n| n == name) {
            return Err(Error::Service(format!("spawn failed for {name} (mock)")));
        }
        self.spawned.lock().unwrap().push(name.to_string());

        // Real abstract notify socket, unique per spawn: respawns of the
        // same service must not collide with the previous (still-bound)
        // address — that would fail every respawn and mask crash loops.
        let pid = std::process::id();
        let seq = MOCK_PID.fetch_add(1, Ordering::Relaxed);
        let addr = format!("@lion-session-mock/{pid}/{name}-{seq}");
        let notify = bind_abstract(&abstract_name_of(&addr))
            .map_err(|e| Error::Io("mock notify bind".into(), e))?;
        self.notify_addr
            .lock()
            .unwrap()
            .insert(name.to_string(), addr);

        let script = self
            .scripts
            .get(name)
            .cloned()
            .unwrap_or(if argv.is_empty() {
                Script::ExitAfter {
                    delay: Duration::ZERO,
                    code: 0,
                    abnormal: false,
                }
            } else {
                Script::Run
            });
        let child = MockChild {
            pid: MOCK_PID.fetch_add(1, Ordering::Relaxed),
            script,
            state: Mutex::new(ChildPhase::Running),
        };
        Ok(Spawned {
            child: Box::new(child),
            notify: Some(notify),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn scripted_exit() {
        let l = MockLauncher::with_scripts(vec![(
            "crasher".into(),
            Script::ExitAfter {
                delay: Duration::ZERO,
                code: 1,
                abnormal: false,
            },
        )]);
        let s = l.spawn("crasher", &["crash".into()], &[]).await.unwrap();
        assert!(s.notify.is_some());
        let info = s.child.wait().await.unwrap();
        assert_eq!(info.code, Some(1));
        assert!(!info.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn run_forever_until_killed() {
        let l = MockLauncher::default();
        let s = l.spawn("svc", &["svc".into()], &[]).await.unwrap();
        // kill immediately; wait observes the kill
        s.child.kill();
        let info = tokio::time::timeout(Duration::from_secs(1), s.child.wait())
            .await
            .expect("wait returns after kill")
            .unwrap();
        assert!(info.abnormal);
        assert_eq!(info.code, None);
    }

    #[tokio::test]
    async fn notify_ready_roundtrip() {
        let l = MockLauncher::default();
        let s = l.spawn("svc", &["svc".into()], &[]).await.unwrap();
        l.make_ready("svc").await.unwrap();
        let mut buf = [0u8; 64];
        let sock = s.notify.unwrap();
        let (n, _) = tokio::time::timeout(Duration::from_secs(2), sock.recv_from(&mut buf))
            .await
            .expect("notify arrives")
            .unwrap();
        assert_eq!(&buf[..n], b"READY=1");
    }

    #[tokio::test]
    async fn spawn_failure_scripted() {
        let l = MockLauncher {
            fail_names: vec!["bad".into()],
            ..Default::default()
        };
        let e = l.spawn("bad", &["x".into()], &[]).await.unwrap_err();
        assert!(matches!(e, Error::Service(_)));
    }

    #[tokio::test]
    async fn mock_logind_records_and_fails() {
        let ok = MockLogind::default();
        crate::ports::Logind::power_off(&ok).await.unwrap();
        crate::ports::Logind::lock(&ok, Some("c2")).await.unwrap();
        assert_eq!(
            ok.calls(),
            vec!["power-off".to_string(), "lock:c2".to_string()]
        );
        let bad = MockLogind {
            fail: true,
            ..Default::default()
        };
        assert!(crate::ports::Logind::suspend(&bad).await.is_err());
    }

    #[tokio::test]
    async fn mock_authorizer_modes() {
        let allow = MockAuthorizer::default();
        assert!(
            crate::ports::Authorizer::authorize(&allow, "session.logout", 1000)
                .await
                .unwrap()
        );
        let deny = MockAuthorizer {
            allow_all: false,
            ..Default::default()
        };
        assert!(
            !crate::ports::Authorizer::authorize(&deny, "power.shutdown", 1000)
                .await
                .unwrap()
        );
        assert!(
            crate::ports::Authorizer::authorize(&deny, "power.shutdown", 0)
                .await
                .unwrap()
        );
    }
}
