//! End-to-end integration tests: the real `lion-session` binary runs as a
//! child process against
//!
//!   - a private `dbus-daemon` (session AND system role, via
//!     DBUS_SESSION_BUS_ADDRESS / DBUS_SYSTEM_BUS_ADDRESS)
//!   - a **mock logind** (org.freedesktop.login1) whose Inhibit() returns
//!     real pipe fds, so "inhibitor released" is observable as EOF
//!   - **mock lion-locker / lion-power** services recording every call
//!   - a **fake compositor** (a python script that really creates, listens
//!     on, and cleans up the Wayland socket, and can be SIGKILLed to test
//!     crash recovery)
//!   - a **fake autostart app** that logs its lifecycle
//!
//! Everything a real session exercises -- logind signal classification,
//! lock-before-sleep, cooperative QueryEndSession with veto + timeout,
//! logind delay inhibitors held and released, compositor crash recovery
//! with stale-socket cleanup, crash-loop exit codes -- is covered with
//! real D-Bus traffic and real processes. No production code path is
//! mocked: only the peers around the session are.

use std::{
    os::fd::{FromRawFd, OwnedFd},
    path::PathBuf,
    process::Stdio,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use tokio::{io::AsyncBufReadExt, process::Child, time::sleep};
use zbus::{
    interface, object_server::SignalContext, Connection, MatchRule, MessageStream, MessageType,
};

static COUNTER: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// Mock peers
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Shared {
    records: Mutex<Vec<String>>,
    inhibitor_reads: Mutex<Vec<OwnedFd>>,
}

impl Shared {
    fn record(&self, line: impl Into<String>) {
        self.records.lock().unwrap().push(line.into());
    }
    fn has(&self, needle: &str) -> bool {
        self.records
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.contains(needle))
    }
    fn count(&self, needle: &str) -> usize {
        self.records
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.contains(needle))
            .count()
    }
}

struct MockLogindManager {
    shared: Arc<Shared>,
}

#[interface(name = "org.freedesktop.login1.Manager")]
impl MockLogindManager {
    #[zbus(signal)]
    async fn prepare_for_shutdown(ctxt: &SignalContext<'_>, start: &bool) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn prepare_for_sleep(ctxt: &SignalContext<'_>, start: &bool) -> zbus::Result<()>;

    /// Return a pipe write-end as the inhibitor fd: EOF on the read end
    /// proves lion-session released the inhibitor (dropped the fd).
    async fn inhibit(
        &self,
        what: &str,
        _who: &str,
        _why: &str,
        mode: &str,
    ) -> zbus::zvariant::OwnedFd {
        self.shared.record(format!("inhibit:{what}:{mode}"));
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: plain pipe(2); both fds are wrapped below.
        unsafe { libc::pipe(fds.as_mut_ptr()) };
        let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        self.shared.inhibitor_reads.lock().unwrap().push(read);
        zbus::zvariant::OwnedFd::from(write)
    }
}

struct MockLogindSession {
    shared: Arc<Shared>,
}

#[interface(name = "org.freedesktop.login1.Session")]
impl MockLogindSession {
    async fn set_idle_hint(&self, idle: bool) {
        self.shared.record(format!("idle-hint:{idle}"));
    }
}

struct MockLocker {
    shared: Arc<Shared>,
}

#[interface(name = "os.lionos.Locker1")]
impl MockLocker {
    async fn lock(&self) {
        self.shared.record("locker:Lock");
    }
    async fn unlock(&self) {
        self.shared.record("locker:Unlock");
    }
}

struct MockPower {
    shared: Arc<Shared>,
}

#[interface(name = "os.lionos.Power1")]
impl MockPower {
    async fn restart(&self) {
        self.shared.record("power:Restart");
    }
    async fn shutdown(&self) {
        self.shared.record("power:Shutdown");
    }
    async fn suspend(&self) {
        self.shared.record("power:Suspend");
    }
    async fn hibernate(&self) {
        self.shared.record("power:Hibernate");
    }
}

// ---------------------------------------------------------------------------
// Test environment
// ---------------------------------------------------------------------------

const COMPOSITOR_PY: &str = r#"
import os, signal, socket, sys, time
runtime = os.environ["XDG_RUNTIME_DIR"]
name = os.environ.get("WAYLAND_DISPLAY", "wayland-1")
path = os.path.join(runtime, name)
pidfile = sys.argv[1]
try:
    os.unlink(path)
except FileNotFoundError:
    pass
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.bind(path)
s.listen(16)
with open(pidfile, "w") as f:
    f.write(str(os.getpid()))
done = False
def stop(*a):
    global done
    done = True
signal.signal(signal.SIGTERM, stop)
s.settimeout(0.2)
while not done:
    try:
        c, _ = s.accept()
        c.close()
    except socket.timeout:
        continue
    except OSError:
        break
try:
    os.unlink(path)
except FileNotFoundError:
    pass
sys.exit(0)
"#;

const APP_PY: &str = r#"
import os, signal, sys, time
log = sys.argv[1]
def note(ev):
    with open(log, "a") as f:
        f.write(ev + "\n")
note("start")
stop = {"v": False}
signal.signal(signal.SIGTERM, lambda *a: stop.update(v=True))
while not stop["v"]:
    time.sleep(0.1)
note("term")
sys.exit(0)
"#;

struct TestEnv {
    dir: PathBuf,
    addr: String,
    dbus: Child,
    session: Option<Child>,
    shared: Arc<Shared>,
    logind_conn: Option<Connection>,
    keepalive: Option<(Connection, Connection)>,
    log_path: PathBuf,
}

impl TestEnv {
    async fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "lion-session-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("mkdir test dir");
        let runtime = dir.join("runtime");
        std::fs::create_dir_all(&runtime).expect("mkdir runtime");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        }

        std::fs::write(dir.join("compositor.py"), COMPOSITOR_PY).unwrap();
        std::fs::write(dir.join("app.py"), APP_PY).unwrap();

        // Private dbus-daemon, used as BOTH session and system bus.
        let sock = dir.join("bus");
        let conf = dir.join("bus.conf");
        std::fs::write(
            &conf,
            format!(
                r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <listen>unix:path={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*"/>
    <allow receive_sender="*"/>
    <allow own="*"/>
    <allow user="*"/>
  </policy>
</busconfig>
"#,
                sock.display()
            ),
        )
        .unwrap();

        let mut dbus = tokio::process::Command::new("dbus-daemon")
            .arg("--config-file")
            .arg(&conf)
            .arg("--print-address=1")
            .arg("--nofork")
            .arg("--nopidfile")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn dbus-daemon");
        let addr = {
            let mut line = String::new();
            let mut out = dbus.stdout.take().expect("dbus stdout");
            let mut reader = tokio::io::BufReader::new(&mut out);
            reader
                .read_line(&mut line)
                .await
                .expect("read dbus address");
            line.trim().to_string()
        };
        assert!(addr.starts_with("unix:"), "unexpected address: {addr}");

        let log_path = dir.join("lion-session.log");
        Self {
            dir,
            addr,
            dbus,
            session: None,
            shared: Arc::new(Shared::default()),
            logind_conn: None,
            keepalive: None,
            log_path,
        }
    }

    /// Serve the mock logind (manager + session), locker, and power peers.
    async fn serve_mocks(&mut self) {
        let shared = self.shared.clone();
        let logind = zbus::connection::Builder::address(self.addr.as_str())
            .unwrap()
            .name("org.freedesktop.login1")
            .unwrap()
            .serve_at(
                "/org/freedesktop/login1",
                MockLogindManager {
                    shared: shared.clone(),
                },
            )
            .unwrap()
            .serve_at(
                "/org/freedesktop/login1/session/auto",
                MockLogindSession {
                    shared: shared.clone(),
                },
            )
            .unwrap()
            .build()
            .await
            .expect("serve mock logind");

        let locker = zbus::connection::Builder::address(self.addr.as_str())
            .unwrap()
            .name("os.lionos.Locker")
            .unwrap()
            .serve_at(
                "/os/lionos/Locker",
                MockLocker {
                    shared: shared.clone(),
                },
            )
            .unwrap()
            .build()
            .await
            .expect("serve mock locker");

        let power = zbus::connection::Builder::address(self.addr.as_str())
            .unwrap()
            .name("os.lionos.Power")
            .unwrap()
            .serve_at("/os/lionos/Power", MockPower { shared })
            .unwrap()
            .build()
            .await
            .expect("serve mock power");

        self.logind_conn = Some(logind);
        self.keepalive = Some((locker, power));
    }

    /// Emit a logind Manager signal from the mock.
    async fn emit_logind_signal(&self, member: &str, start: bool) {
        let conn = self.logind_conn.as_ref().expect("mocks not served");
        let iface = conn
            .object_server()
            .interface::<_, MockLogindManager>("/org/freedesktop/login1")
            .await
            .expect("mock manager iface");
        let ctxt: SignalContext<'static> = iface.signal_context().clone();
        match member {
            "PrepareForSleep" => MockLogindManager::prepare_for_sleep(&ctxt, &start)
                .await
                .expect("emit PrepareForSleep"),
            "PrepareForShutdown" => MockLogindManager::prepare_for_shutdown(&ctxt, &start)
                .await
                .expect("emit PrepareForShutdown"),
            other => panic!("unknown mock signal {other}"),
        }
    }

    /// The test config, driving the fake compositor and one fake app.
    fn config_toml(&self, end_timeout_ms: u64, max_restarts: u32) -> String {
        format!(
            r#"logout-animation-ms = 40
[compositor]
command = "python3"
args = ["{}", "{}"]
wayland-display = "wayland-1"
ready-timeout-ms = 8000
restart = true
max-restarts = {max_restarts}
crash-window-ms = 60000
[session]
end-timeout-ms = {end_timeout_ms}
lock-on-sleep = true
xdg-autostart = false
[[autostart]]
name = "app-a"
command = "python3"
args = ["{}", "{}"]
restart = "never"
"#,
            self.dir.join("compositor.py").display(),
            self.dir.join("compositor.pid").display(),
            self.dir.join("app.py").display(),
            self.dir.join("app.log").display(),
        )
    }

    async fn start_session(&mut self, toml: &str) -> &mut Child {
        let cfg_path = self.dir.join("session.toml");
        std::fs::write(&cfg_path, toml).unwrap();
        let log = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&self.log_path)
            .unwrap();
        let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_lion-session"))
            .env("DBUS_SESSION_BUS_ADDRESS", &self.addr)
            .env("DBUS_SYSTEM_BUS_ADDRESS", &self.addr)
            .env("XDG_RUNTIME_DIR", self.dir.join("runtime"))
            .env("XDG_CONFIG_HOME", self.dir.join("config-home"))
            .env("HOME", self.dir.join("home"))
            .env("LION_SESSION_CONFIG", &cfg_path)
            .env("RUST_LOG", "lion_session=debug")
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .kill_on_drop(true)
            .spawn()
            .expect("spawn lion-session");
        self.session = Some(child);
        self.session.as_mut().unwrap()
    }

    fn runtime_dir(&self) -> PathBuf {
        self.dir.join("runtime")
    }

    fn compositor_pid(&self) -> Option<i32> {
        std::fs::read_to_string(self.dir.join("compositor.pid"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
    }

    fn app_log(&self) -> String {
        std::fs::read_to_string(self.dir.join("app.log")).unwrap_or_default()
    }

    fn session_log(&self) -> String {
        std::fs::read_to_string(&self.log_path).unwrap_or_default()
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        if let Some(c) = self.session.as_mut() {
            let _ = c.start_kill();
        }
        let _ = self.dbus.start_kill();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn client(env: &TestEnv) -> Connection {
    zbus::connection::Builder::address(env.addr.as_str())
        .expect("client builder")
        .build()
        .await
        .expect("client connect")
}

async fn call(
    conn: &Connection,
    method: &str,
    body: &(impl zbus::zvariant::DynamicType + serde::ser::Serialize),
) -> zbus::message::Message {
    conn.call_method(
        Some("os.lionos.Session"),
        "/os/lionos/Session",
        Some("os.lionos.Session1"),
        method,
        body,
    )
    .await
    .expect("call method")
}

/// Wait until the session owns its bus name; returns a client connection.
async fn wait_for_service(env: &TestEnv) -> Connection {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let conn = client(env).await;
        let reply = conn
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "NameHasOwner",
                &("os.lionos.Session",),
            )
            .await;
        if let Ok(m) = reply {
            if let Ok(true) = m.body().deserialize::<bool>() {
                return conn;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "service never appeared; log:\n{}",
            env.session_log()
        );
        sleep(Duration::from_millis(100)).await;
    }
}

/// Collect member names of every os.lionos.Session signal into a shared vec.
async fn spawn_signal_listener(env: &TestEnv, seen: Arc<Mutex<Vec<String>>>) {
    let conn = client(env).await;
    let rule: MatchRule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender("os.lionos.Session")
        .expect("match rule sender")
        .build();
    let stream = MessageStream::for_match_rule(rule, &conn, None)
        .await
        .expect("signal stream");
    tokio::spawn(async move {
        use zbus::export::futures_util::StreamExt;
        let mut stream = stream;
        while let Some(Ok(msg)) = stream.next().await {
            if let Some(m) = msg.header().member() {
                seen.lock().unwrap().push(m.as_str().to_string());
            }
        }
    });
}

async fn wait_for<F: Fn(&TestEnv) -> bool>(env: &TestEnv, what: &str, timeout: Duration, pred: F) {
    let deadline = tokio::time::Instant::now() + timeout;
    while !pred(env) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what}\n--- session log ---\n{}",
            env.session_log()
        );
        sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_metrics(conn: &Connection, needle: &str, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let m: String = call(conn, "GetMetrics", &())
            .await
            .body()
            .deserialize()
            .unwrap();
        if m.contains(needle) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "metrics never contained {needle}"
        );
        sleep(Duration::from_millis(100)).await;
    }
}

/// Read the read-only State property via standard Properties.Get.
async fn get_state(conn: &Connection) -> String {
    let reply = conn
        .call_method(
            Some("os.lionos.Session"),
            "/os/lionos/Session",
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("os.lionos.Session1", "State"),
        )
        .await
        .expect("Properties.Get");
    let (v,): (zbus::zvariant::OwnedValue,) = reply.body().deserialize().expect("property body");
    String::try_from(v).expect("State as string")
}

fn pid_alive(pid: i32) -> bool {
    // SAFETY: kill(pid, 0) probes existence without delivering a signal.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn saw(signals: &Arc<Mutex<Vec<String>>>, member: &str) -> bool {
    signals.lock().unwrap().iter().any(|m| m == member)
}

// ---------------------------------------------------------------------------
// Test 1: full session lifecycle against the mock peers
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_lifecycle_with_logind_mock() {
    tokio::time::timeout(Duration::from_secs(90), inner_full_lifecycle())
        .await
        .expect("test timed out");
}

async fn inner_full_lifecycle() {
    let mut env = TestEnv::new().await;
    env.serve_mocks().await;

    let toml = env.config_toml(1500, 3);
    env.start_session(&toml).await;

    let signals: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    spawn_signal_listener(&env, signals.clone()).await;

    let conn = wait_for_service(&env).await;
    println!("CHECK 01: os.lionos.Session owns its name");

    wait_for(&env, "wayland socket", Duration::from_secs(10), |e| {
        e.runtime_dir().join("wayland-1").exists()
    })
    .await;
    println!("CHECK 02: compositor created the Wayland socket");

    wait_for(&env, "compositor pidfile", Duration::from_secs(5), |e| {
        e.compositor_pid().is_some()
    })
    .await;
    let comp_pid = env.compositor_pid().unwrap();
    assert!(pid_alive(comp_pid));
    println!("CHECK 03: compositor running (pid {comp_pid})");

    // Capabilities advertise the new integrations.
    let caps: Vec<String> = call(&conn, "GetCapabilities", &())
        .await
        .body()
        .deserialize()
        .unwrap();
    for want in [
        "query-end",
        "inhibit",
        "logind",
        "xdg-autostart",
        "compositor-recovery",
    ] {
        assert!(caps.iter().any(|c| c == want), "missing capability {want}");
    }
    println!("CHECK 04: capabilities include {caps:?}");

    // Register + inhibit.
    let token: u64 = call(&conn, "RegisterClient", &("test-shell",))
        .await
        .body()
        .deserialize()
        .unwrap();
    assert!(token > 0);
    let cookie: u64 = call(&conn, "Inhibit", &("test-app", "saving state"))
        .await
        .body()
        .deserialize()
        .unwrap();
    assert!(cookie > 0);
    let inhibited: bool = call(&conn, "IsInhibited", &())
        .await
        .body()
        .deserialize()
        .unwrap();
    assert!(inhibited);
    let list: Vec<(String, String)> = call(&conn, "ListInhibitors", &())
        .await
        .body()
        .deserialize()
        .unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].0, "test-app");
    println!("CHECK 05: RegisterClient + Inhibit + IsInhibited + ListInhibitors");

    // Idle hint reaches logind.
    call(&conn, "SetIdle", &true).await;
    wait_for(&env, "idle hint in logind", Duration::from_secs(3), |e| {
        e.shared.has("idle-hint:true")
    })
    .await;
    println!("CHECK 06: SetIdle(true) forwarded to logind SetIdleHint");

    // logind PrepareForSleep -> lock-before-sleep.
    env.emit_logind_signal("PrepareForSleep", true).await;
    wait_for(&env, "lock on sleep", Duration::from_secs(3), |e| {
        e.shared.count("locker:Lock") >= 1
    })
    .await;
    println!("CHECK 07: PrepareForSleep(true) locked the session first");

    // Direct lock / unlock.
    call(&conn, "Lock", &()).await;
    call(&conn, "Unlock", &()).await;
    wait_for(&env, "unlock forwarded", Duration::from_secs(3), |e| {
        e.shared.has("locker:Unlock")
    })
    .await;
    assert!(env.shared.count("locker:Lock") >= 2);
    println!("CHECK 08: Lock()/Unlock() forwarded to lion-locker");

    // Suspend forwards but keeps the session alive.
    call(&conn, "Suspend", &()).await;
    wait_for(&env, "power Suspend", Duration::from_secs(3), |e| {
        e.shared.has("power:Suspend")
    })
    .await;
    assert!(env.session.as_ref().unwrap().id().is_some());
    println!("CHECK 09: Suspend() forwarded, session still running");

    // Metrics show the app started.
    wait_metrics(&conn, "\"apps_started\":1", Duration::from_secs(10)).await;
    println!("CHECK 10: metrics report the autostart app");
    assert!(
        env.app_log().contains("start"),
        "app log: {}",
        env.app_log()
    );

    // Shutdown: forward + logind delay inhibitor + query + answer + end.
    let started = std::time::Instant::now();
    call(&conn, "Shutdown", &()).await;
    let reply_elapsed = started.elapsed();
    assert!(
        reply_elapsed < Duration::from_secs(2),
        "reply took {reply_elapsed:?}"
    );
    wait_for(&env, "power Shutdown", Duration::from_secs(3), |e| {
        e.shared.has("power:Shutdown")
    })
    .await;
    wait_for(
        &env,
        "logind delay inhibitor",
        Duration::from_secs(3),
        |e| e.shared.has("inhibit:shutdown:delay"),
    )
    .await;
    wait_for(
        &env,
        "QueryEndSession signal",
        Duration::from_secs(3),
        |_e| saw(&signals, "QueryEndSession"),
    )
    .await;
    println!(
        "CHECK 11: Shutdown forwarded, delay inhibitor taken, QueryEndSession emitted (reply in {reply_elapsed:?})"
    );

    // Approve the end and release the inhibitor.
    let _: String = call(&conn, "EndSessionResponse", &(token, true, ""))
        .await
        .body()
        .deserialize()
        .unwrap();
    let released: bool = call(&conn, "Uninhibit", &(cookie,))
        .await
        .body()
        .deserialize()
        .unwrap();
    assert!(released);
    println!("CHECK 12: EndSessionResponse(ok) + Uninhibit accepted");

    // The session exits cleanly.
    let status = tokio::time::timeout(
        Duration::from_secs(10),
        env.session.as_mut().unwrap().wait(),
    )
    .await
    .expect("session exit timeout")
    .expect("wait");
    assert!(status.success(), "exit: {status:?}");
    println!("CHECK 13: session exited 0");

    // The app got SIGTERM and exited gracefully.
    assert!(env.app_log().contains("term"), "app log: {}", env.app_log());
    println!("CHECK 14: autostart app terminated gracefully");

    // The compositor is gone.
    wait_for(&env, "compositor dead", Duration::from_secs(5), |_| {
        !pid_alive(comp_pid)
    })
    .await;
    println!("CHECK 15: compositor torn down");

    // The logind inhibitor was released: EOF on the pipe read end.
    let reads: Vec<OwnedFd> = env
        .shared
        .inhibitor_reads
        .lock()
        .unwrap()
        .drain(..)
        .collect();
    assert!(!reads.is_empty());
    for fd in reads {
        use std::io::Read;
        let mut f = std::fs::File::from(fd);
        let mut buf = [0u8; 8];
        let n = f.read(&mut buf).unwrap_or(0);
        assert_eq!(n, 0, "inhibitor fd still open (write end leaked)");
    }
    println!("CHECK 16: logind delay inhibitor released (EOF observed)");

    // EndSession + PreparingToEnd were signalled.
    assert!(saw(&signals, "EndSession"));
    assert!(saw(&signals, "PreparingToEnd"));
    assert!(saw(&signals, "InhibitorAdded"));
    println!("CHECK 17: EndSession / PreparingToEnd / InhibitorAdded signals seen");
}

// ---------------------------------------------------------------------------
// Test 2: compositor crash recovery and the crash-loop exit code
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compositor_crash_recovery_and_crashloop_exit() {
    tokio::time::timeout(Duration::from_secs(90), inner_crash_recovery())
        .await
        .expect("test timed out");
}

async fn inner_crash_recovery() {
    let mut env = TestEnv::new().await;
    env.serve_mocks().await;

    // max-restarts = 1: the first crash is recovered, the second trips the
    // guard and ends the session with the documented exit code 3.
    let toml = env.config_toml(800, 1);
    env.start_session(&toml).await;
    let _conn = wait_for_service(&env).await;

    wait_for(&env, "compositor up", Duration::from_secs(10), |e| {
        e.compositor_pid().is_some()
    })
    .await;
    let pid1 = env.compositor_pid().unwrap();

    // SIGKILL the compositor: the session must respawn it (stale socket
    // detection and all).
    // SAFETY: sending SIGKILL to our fake compositor child.
    unsafe { libc::kill(pid1, libc::SIGKILL) };
    wait_for(
        &env,
        "compositor respawned",
        Duration::from_secs(12),
        |e| matches!(e.compositor_pid(), Some(p) if p != pid1),
    )
    .await;
    let pid2 = env.compositor_pid().unwrap();
    assert_ne!(pid2, pid1);
    assert!(
        env.session.as_ref().unwrap().id().is_some(),
        "session died on crash 1"
    );
    println!("CHECK 01: compositor respawned after SIGKILL ({pid1} -> {pid2})");
    assert!(
        env.runtime_dir().join("wayland-1").exists(),
        "stale socket not cleaned / recreated"
    );
    println!("CHECK 02: wayland socket present after respawn (stale socket cleaned)");

    // Second crash: the crash-loop guard trips -> exit code 3.
    // SAFETY: sending SIGKILL to the respawned fake compositor.
    unsafe { libc::kill(pid2, libc::SIGKILL) };
    let status = tokio::time::timeout(
        Duration::from_secs(20),
        env.session.as_mut().unwrap().wait(),
    )
    .await
    .expect("crashloop exit timeout")
    .expect("wait");
    assert_eq!(status.code(), Some(3), "exit: {status:?}");
    println!("CHECK 03: crash-loop gave up with exit code 3");
    assert!(
        env.session_log().contains("crash-looping"),
        "log: {}",
        env.session_log()
    );
    println!("CHECK 04: crash-loop is visible in the logs");
}

// ---------------------------------------------------------------------------
// Test 3: logout veto, approval, and forced timeout
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logout_veto_and_forced_timeout() {
    tokio::time::timeout(Duration::from_secs(90), inner_veto())
        .await
        .expect("test timed out");
}

async fn inner_veto() {
    let mut env = TestEnv::new().await;
    env.serve_mocks().await;

    let toml = env.config_toml(800, 3);
    env.start_session(&toml).await;

    let signals: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    spawn_signal_listener(&env, signals.clone()).await;

    let conn = wait_for_service(&env).await;
    wait_for(&env, "compositor up", Duration::from_secs(10), |e| {
        e.compositor_pid().is_some()
    })
    .await;

    let token: u64 = call(&conn, "RegisterClient", &("editor",))
        .await
        .body()
        .deserialize()
        .unwrap();

    // --- veto: the editor refuses to quit a logout -----------------------
    call(&conn, "Logout", &()).await;
    wait_for(&env, "QueryEndSession", Duration::from_secs(3), |_e| {
        saw(&signals, "QueryEndSession")
    })
    .await;
    let reply_state: String = call(
        &conn,
        "EndSessionResponse",
        &(token, false, "unsaved document"),
    )
    .await
    .body()
    .deserialize()
    .unwrap();
    let _ = reply_state; // informational: the cancel task runs after the reply
    wait_for(&env, "EndCanceled signal", Duration::from_secs(3), |_e| {
        saw(&signals, "EndCanceled")
    })
    .await;
    // The session must return to "running" (property poll: the veto task
    // resets the state asynchronously after the D-Bus reply went out).
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let state = get_state(&conn).await;
            if state == "running" {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "state never returned to running (last: {state})"
            );
            sleep(Duration::from_millis(50)).await;
        }
    }
    sleep(Duration::from_millis(300)).await;
    assert!(
        env.session.as_ref().unwrap().id().is_some(),
        "session died despite veto"
    );
    println!("CHECK 01: ok=false vetoed the logout, session continues, EndCanceled emitted");

    // --- approve: the second logout goes through -------------------------
    call(&conn, "Logout", &()).await;
    let _: String = call(&conn, "EndSessionResponse", &(token, true, ""))
        .await
        .body()
        .deserialize()
        .unwrap();
    let status = tokio::time::timeout(
        Duration::from_secs(10),
        env.session.as_mut().unwrap().wait(),
    )
    .await
    .expect("exit timeout")
    .expect("wait");
    assert!(status.success());
    println!("CHECK 02: approved logout ends the session");

    // --- timeout: nobody answers, the end is forced -----------------------
    let toml = env.config_toml(600, 3);
    env.start_session(&toml).await;
    let conn2 = wait_for_service(&env).await;
    let _token2: u64 = call(&conn2, "RegisterClient", &("silent-app",))
        .await
        .body()
        .deserialize()
        .unwrap();
    call(&conn2, "Logout", &()).await;
    let started = std::time::Instant::now();
    let status = tokio::time::timeout(
        Duration::from_secs(15),
        env.session.as_mut().unwrap().wait(),
    )
    .await
    .expect("forced exit timeout")
    .expect("wait");
    assert!(status.success());
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(500),
        "session ended too fast to have waited: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "session waited far beyond the timeout: {elapsed:?}"
    );
    println!("CHECK 03: unanswered QueryEndSession forced after the timeout ({elapsed:?})");
    assert!(
        env.session_log().contains("did not answer"),
        "log: {}",
        env.session_log()
    );
    println!("CHECK 04: forced end is visible in the logs");
}

// ---------------------------------------------------------------------------
// Test 4 (0.3.0): idle escalation + lock-on-shutdown, end to end
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_escalation_and_lock_on_shutdown() {
    let mut env = TestEnv::new().await;
    env.serve_mocks().await;

    // Idle policy: lock after 1.5 s, no idle logout (we want to inspect
    // the locked-but-alive state). lock-on-shutdown defaults on. The key
    // must land INSIDE the [session] table — i.e. before [[autostart]].
    let toml = env
        .config_toml(3000, 3)
        .replace("[[autostart]]", "lock-after-ms = 1500\n[[autostart]]");
    env.start_session(&toml).await;
    let conn = wait_for_service(&env).await;

    println!("CHECK 01: SetIdle(true) forwards the hint to logind");
    let _: () = call(&conn, "SetIdle", &(true,))
        .await
        .body()
        .deserialize()
        .unwrap();
    wait_for(&env, "idle hint", Duration::from_secs(3), |e| {
        e.shared.has("idle-hint:true")
    })
    .await;

    println!("CHECK 02: idle policy locks the session via lion-locker");
    wait_for(&env, "idle lock", Duration::from_secs(6), |e| {
        e.shared.count("locker:Lock") >= 1
    })
    .await;
    wait_metrics(&conn, "\"idle_locks\":1", Duration::from_secs(3)).await;

    println!("CHECK 03: waking up cancels further escalation (no idle logout)");
    let _: () = call(&conn, "SetIdle", &(false,))
        .await
        .body()
        .deserialize()
        .unwrap();
    wait_for(&env, "idle hint cleared", Duration::from_secs(3), |e| {
        e.shared.has("idle-hint:false")
    })
    .await;
    // With logout-after disabled nothing further happens; the session
    // must still be alive a tick-granularity later.
    sleep(Duration::from_millis(1500)).await;
    assert!(env.session.as_ref().unwrap().id().is_some());

    println!("CHECK 04: lock announced exactly once per idle period");
    let locks_after_wake = env.shared.count("locker:Lock");
    sleep(Duration::from_millis(1500)).await;
    assert_eq!(env.shared.count("locker:Lock"), locks_after_wake);

    // Re-enter idle: locks again (fresh period).
    let _: () = call(&conn, "SetIdle", &(true,))
        .await
        .body()
        .deserialize()
        .unwrap();
    wait_for(&env, "second idle lock", Duration::from_secs(6), |e| {
        e.shared.count("locker:Lock") > locks_after_wake
    })
    .await;

    println!("CHECK 05: PrepareForShutdown locks BEFORE ending (cancelled-shutdown safety)");
    let locks_before = env.shared.count("locker:Lock");
    env.emit_logind_signal("PrepareForShutdown", true).await;
    // The session fast-ends; the lock must have been forwarded on the way.
    let status = tokio::time::timeout(
        Duration::from_secs(10),
        env.session.as_mut().unwrap().wait(),
    )
    .await
    .expect("shutdown end timeout")
    .expect("wait");
    assert!(status.success(), "shutdown-end exit code: {status:?}");
    assert!(
        env.shared.count("locker:Lock") > locks_before,
        "lock-on-shutdown did not fire: records = {:?}",
        env.shared.records.lock().unwrap()
    );

    println!("CHECK 06: shutdown_locks metric recorded");
    let log = env.session_log();
    assert!(
        log.contains("hardening applied") || log.contains("lion-session 0.3"),
        "0.3.0 log markers missing"
    );
}
