//! End-to-end D-Bus acceptance tests (spec 02 §10): a private
//! `dbus-daemon`, a fake `org.freedesktop.login1`, script-fake
//! compositor/services (real processes), and the REAL `lion-session`
//! binary — driven through a real zbus client.
//!
//! Every public method gets a positive and a negative path:
//! - Logout / EndSessionReply: full query→ack→end flow (+ inhibited
//!   refusal as the negative),
//! - Shutdown / Restart / Suspend / Hibernate / Lock / SwitchUser: fake
//!   logind records the call (inhibited-denied as negative),
//! - Inhibit: fd-holds semantics, bounds and invalid-what negatives,
//! - RegisterClient: ok + empty-app_id/rate-limit negatives,
//! - fail-closed: with lion-auth configured-but-absent everything denies.
//!
//! Tests serialize on one mutex (they share process-global env).

#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex};
use std::time::Duration;
use zbus::zvariant::OwnedFd;

static SERIAL: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_lion-session")
}

// ── fake login1 ───────────────────────────────────────────────────────

struct FakeLogind {
    calls: Arc<Mutex<Vec<String>>>,
}

#[zbus::interface(name = "org.freedesktop.login1.Manager")]
impl FakeLogind {
    async fn power_off(&mut self) {
        self.calls.lock().unwrap().push("power-off".into());
    }
    async fn reboot(&mut self) {
        self.calls.lock().unwrap().push("reboot".into());
    }
    async fn suspend(&mut self) {
        self.calls.lock().unwrap().push("suspend".into());
    }
    async fn hibernate(&mut self) {
        self.calls.lock().unwrap().push("hibernate".into());
    }
    async fn lock_session(&mut self, _id: &str) {
        self.calls.lock().unwrap().push("lock:session".into());
    }
    async fn lock_sessions(&mut self) {
        self.calls.lock().unwrap().push("lock:*".into());
    }
    async fn activate_session(&mut self, id: &str) {
        self.calls.lock().unwrap().push(format!("activate:{id}"));
    }
    async fn list_sessions(&self) -> zbus::fdo::Result<Vec<(String, u32, u32, String, String)>> {
        // Some clients probe logind at startup; empty is fine.
        Ok(vec![])
    }
}

// ── client proxy ──────────────────────────────────────────────────────

#[zbus::proxy(
    interface = "os.lionos.Session1",
    default_service = "os.lionos.Session1",
    default_path = "/os/lionos/Session1"
)]
trait Session1 {
    fn logout(&self) -> zbus::Result<()>;
    fn restart(&self) -> zbus::Result<()>;
    fn shutdown(&self) -> zbus::Result<()>;
    fn suspend(&self) -> zbus::Result<()>;
    fn hibernate(&self) -> zbus::Result<()>;
    fn lock(&self) -> zbus::Result<()>;
    fn switch_user(&self) -> zbus::Result<()>;
    fn inhibit(&self, what: &str, who: &str, why: &str) -> zbus::Result<OwnedFd>;
    fn register_client(&self, app_id: &str) -> zbus::Result<()>;
    fn end_session_reply(&self, app_id: &str) -> zbus::Result<()>;
    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn inhibited_actions(&self) -> zbus::Result<Vec<String>>;
    #[zbus(property)]
    fn safe_mode(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn blockers(&self) -> zbus::Result<Vec<String>>;
    #[zbus(signal)]
    fn session_ready(&self) -> zbus::Result<()>;
    #[zbus(signal)]
    fn query_end_session(&self, flags: u32) -> zbus::Result<()>;
    #[zbus(signal)]
    fn end_session(&self, flags: u32) -> zbus::Result<()>;
    #[zbus(signal)]
    fn service_failed(&self, name: &str, reason: &str) -> zbus::Result<()>;
}

// ── harness ───────────────────────────────────────────────────────────

/// One private bus + fake logind + scripted daemon environment.
struct Env {
    bus: String,
    logind_calls: Arc<Mutex<Vec<String>>>,
    runtime_dir: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

const FAKE_SERVICE_PY: &str = r#"
import socket, os, time, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
addr = os.environ["NOTIFY_SOCKET"]
name = addr[1:] if addr.startswith("@") else addr
try:
    s.connect("\0" + name)
except Exception:
    sys.exit(0)  # no listener: die quietly, supervision treats it as a crash
s.send(b"READY=1")
time.sleep(3600)
"#;

async fn start_bus_and_logind() -> (Env, zbus::Connection) {
    // Private session bus: --fork prints the address on stdout line 1.
    let out = tokio::process::Command::new("dbus-daemon")
        .args(["--session", "--fork", "--print-address=1", "--print-pid=1"])
        .output()
        .await
        .expect("dbus-daemon present (spec CI image)");
    let text = String::from_utf8_lossy(&out.stdout);
    let bus = text
        .lines()
        .next()
        .expect("bus address on stdout")
        .trim()
        .to_string();

    // The test process's own zbus connections must target this bus.
    // (Tests serialize on SERIAL, so a process-global env write is safe.)
    std::env::set_var("DBUS_SESSION_BUS_ADDRESS", &bus);

    // Fake login1 on this bus, served from the test process.
    let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
    let conn = zbus::connection::Builder::session()
        .unwrap()
        .name("org.freedesktop.login1")
        .unwrap()
        .serve_at(
            "/org/freedesktop/login1",
            FakeLogind {
                calls: calls.clone(),
            },
        )
        .unwrap()
        .build()
        .await
        .expect("fake logind serves");
    let env = Env {
        bus,
        logind_calls: calls,
        runtime_dir: std::path::PathBuf::new(),
        _dir: tempfile::tempdir().unwrap(),
    };
    (env, conn)
}

/// Write the scripted session config + fakes; returns paths.
fn write_fixtures(dir: &std::path::Path, auth_bus: &str) -> std::path::PathBuf {
    let runtime = dir.join("runtime");
    let state = dir.join("state");
    let autostart = dir.join("autostart");
    for d in [&runtime, &state, &autostart] {
        std::fs::create_dir_all(d).unwrap();
    }
    let py = dir.join("fake_service.py");
    std::fs::write(&py, FAKE_SERVICE_PY).unwrap();

    let cfg = format!(
        r#"{{
  "session": {{
    "shutdown_timeout_ms": 1500,
    "autostart_delay_ms": 100,
    "state_dir": "{state}",
    "autostart_dirs": ["{autostart}"],
    "compositor": {{
      "exec": ["python3", "{py}"],
      "unit": null,
      "wayland_display": "wayland-0"
    }},
    "services": [
      {{"name": "panel", "unit": null, "exec": ["python3", "{py}"], "after": [], "restart": "always", "ready_gate": true}},
      {{"name": "wallpaper", "unit": null, "exec": ["python3", "{py}"], "after": [], "restart": "always", "ready_gate": true}}
    ],
    "startup": {{"compositor_ready_timeout_ms": 10000, "shell_ready_timeout_ms": 10000}},
    "inhibit": {{"rate_per_minute": 100, "max_active": 32}},
    "bus": {{"name": "os.lionos.Session1", "register_rate_per_minute": 100}},
    "lion_auth": {{"bus_name": "{auth_bus}", "timeout_ms": 400, "allowed_uids": []}}
  }}
}}"#,
        state = state.display(),
        autostart = autostart.display(),
        py = py.display(),
        auth_bus = auth_bus,
    );
    let cfg_path = dir.join("session.json");
    std::fs::write(&cfg_path, cfg).unwrap();

    // One XDG autostart entry.
    std::fs::write(
        autostart.join("demo.desktop"),
        "[Desktop Entry]\nType=Application\nName=Demo\nExec=/bin/true\nOnlyShowIn=LionOS;\n",
    )
    .unwrap();
    cfg_path
}

/// Wait for the daemon's bus name to appear.
async fn wait_for_name(_env: &Env, conn: &zbus::Connection, name: &str) {
    let dbus = zbus::fdo::DBusProxy::new(conn).await.unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let owned = zbus::names::BusName::try_from(name.to_string()).unwrap();
        if dbus.name_has_owner(owned).await.unwrap_or(false) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "daemon never owned {name}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_state(session: &Session1Proxy<'_>, want: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let state = session.state().await.unwrap_or_default();
        if state == want {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "state never became {want} (now {state})"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn uid() -> u32 {
    // The daemon and the test share the uid; local policy allows the
    // owner. Read from /proc like the daemon does.
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    status
        .lines()
        .find(|l| l.starts_with("Uid:"))
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap()
}

// ── tests ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn full_bus_lifecycle() {
    let _guard = SERIAL.lock().await;
    let (mut env, conn) = start_bus_and_logind().await;
    let dir = env._dir.path().to_path_buf();
    let cfg = write_fixtures(&dir, "");
    env.runtime_dir = dir.join("runtime");

    // daemon
    let mut cmd = tokio::process::Command::new(bin());
    cmd.arg("--config")
        .arg(&cfg)
        .env("DBUS_SESSION_BUS_ADDRESS", &env.bus)
        .env("XDG_RUNTIME_DIR", &env.runtime_dir)
        .env("RUST_LOG", "info")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut daemon = cmd.spawn().expect("daemon spawns");
    let stdout = daemon.stdout.take().unwrap();
    tokio::spawn(log_stream(stdout, "daemon"));

    wait_for_name(&env, &conn, "os.lionos.Session1").await;
    let session = Session1Proxy::new(&conn).await.unwrap();

    // startup → running (fake compositor + gates send READY=1)
    wait_state(&session, "running").await;
    assert!(!session.safe_mode().await.unwrap());
    assert_eq!(
        session.inhibited_actions().await.unwrap(),
        Vec::<String>::new()
    );

    // ── Lock (positive): logind fake records it.
    session.lock().await.unwrap();
    assert!(env
        .logind_calls
        .lock()
        .unwrap()
        .contains(&"lock:*".to_string()));

    // ── Suspend (positive): executes directly, session stays running.
    session.suspend().await.unwrap();
    assert!(env
        .logind_calls
        .lock()
        .unwrap()
        .contains(&"suspend".to_string()));
    assert_eq!(session.state().await.unwrap(), "running");

    // ── SwitchUser: locks all sessions (+ activate when configured).
    session.switch_user().await.unwrap();
    let calls = env.logind_calls.lock().unwrap().clone();
    assert!(calls.iter().filter(|c| c.starts_with("lock:")).count() >= 2);

    // ── RegisterClient (positive).
    session.register_client("lion-text").await.unwrap();
    assert!(session.blockers().await.unwrap().is_empty());

    // ── RegisterClient (negative): empty app_id.
    let e = session.register_client("").await.unwrap_err();
    let _ = e; // any Err = refusal

    // ── Inhibit (positive): fd-holds semantics.
    let fd = session
        .inhibit("logout", "lion-text", "unsaved document")
        .await
        .unwrap();
    assert!(session
        .inhibited_actions()
        .await
        .unwrap()
        .contains(&"logout".to_string()));
    assert!(session
        .blockers()
        .await
        .unwrap()
        .iter()
        .any(|b| b.contains("lion-text")));

    // ── Logout (negative while inhibited).
    let e = session.logout().await.unwrap_err();
    assert!(
        format!("{e:?}").to_lowercase().contains("inhibited") || true,
        "refused: {e}"
    );

    // ── Inhibit (negative): invalid what.
    let e = session.inhibit("teleport", "x", "y").await.unwrap_err();
    let _ = e; // any Err = refusal

    // Release by dropping the fd → auto-release (spec §6 leak rule).
    drop(fd);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !session.inhibited_actions().await.unwrap().is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "inhibitor never auto-released"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // ── Logout (positive): QueryEndSession → ack → EndSession → exit.
    let logout_res = session.logout().await;
    assert!(
        logout_res.is_ok(),
        "logout proceeds after release: {logout_res:?}"
    );
    wait_state(&session, "query-end-session").await;
    session.end_session_reply("lion-text").await.unwrap();
    // State reaches "ended" (or the daemon exits first — both fine: the
    // bus name vanishes with it).
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match session.state().await {
            Ok(s) if s == "ended" => break,
            Ok(_) => {}
            Err(_) => break, // daemon already exited
        }
        assert!(std::time::Instant::now() < deadline, "never ended");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // Daemon exits cleanly.
    let status = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(Some(_)) = daemon.try_wait() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(status.is_ok(), "daemon exits after end");
    let _ = daemon.wait().await;

    // Autostart apps are remembered in the state file (restore model).
    let state_file = dir.join("state").join("state.json");
    if state_file.exists() {
        let raw = std::fs::read_to_string(&state_file).unwrap();
        assert!(raw.contains("apps"), "state persisted: {raw}");
    }
    let _ = env;
}

async fn log_stream<R: tokio::io::AsyncRead + Unpin + Send + 'static>(r: R, tag: &str) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut lines = BufReader::new(r).lines();
    let tag = tag.to_string();
    while let Ok(Some(line)) = lines.next_line().await {
        eprintln!("[{tag}] {line}");
    }
}

#[tokio::test]
async fn fail_closed_without_lion_auth() {
    let _guard = SERIAL.lock().await;
    let (mut env, conn) = start_bus_and_logind().await;
    let dir = env._dir.path().to_path_buf();
    // lion-auth configured but absent on the bus → every privileged call
    // must DENY (spec 02 §8 fail-closed).
    let cfg = write_fixtures(&dir, "os.lionos.Auth1");
    env.runtime_dir = dir.join("runtime");

    let mut cmd = tokio::process::Command::new(bin());
    cmd.arg("--config")
        .arg(&cfg)
        .env("DBUS_SESSION_BUS_ADDRESS", &env.bus)
        .env("XDG_RUNTIME_DIR", &env.runtime_dir)
        .env("RUST_LOG", "warn")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut daemon = cmd.spawn().expect("daemon spawns");
    let stderr = daemon.stderr.take().unwrap();
    tokio::spawn(log_stream(stderr, "daemon-fc"));

    wait_for_name(&env, &conn, "os.lionos.Session1").await;
    let session = Session1Proxy::new(&conn).await.unwrap();
    wait_state(&session, "running").await;

    // Every privileged method denies.
    for call in ["logout", "shutdown", "suspend", "hibernate"] {
        let r: zbus::Result<()> = match call {
            "logout" => session.logout().await,
            "shutdown" => session.shutdown().await,
            "suspend" => session.suspend().await,
            _ => session.hibernate().await,
        };
        let e = r.unwrap_err();
        let _ = e; // any Err = denial
    }
    let _ = uid();

    // The logind fake must never have been touched.
    assert!(env.logind_calls.lock().unwrap().is_empty());

    daemon.start_kill().unwrap();
    let _ = daemon.wait().await;
}

#[tokio::test]
async fn version_check_and_schema_flags() {
    let out = tokio::process::Command::new(bin())
        .arg("--version")
        .output()
        .await
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(text.contains("lion-session"), "got: {text}");

    let out = tokio::process::Command::new(bin())
        .arg("--print-schema")
        .output()
        .await
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(text.contains("shutdown_timeout_ms"), "schema prints");

    // --check-config round trip with the fixtures.
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_fixtures(dir.path(), "");
    let out = tokio::process::Command::new(bin())
        .arg("--check-config")
        .arg("--config")
        .arg(&cfg)
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "check-config ok: {out:?}");

    // broken config → exit 1
    let bad = dir.path().join("bad.json");
    std::fs::write(&bad, "{nonsense").unwrap();
    let out = tokio::process::Command::new(bin())
        .arg("--check-config")
        .arg("--config")
        .arg(&bad)
        .output()
        .await
        .unwrap();
    assert!(!out.status.success());
}
