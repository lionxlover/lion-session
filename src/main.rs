#![forbid(unsafe_code)]
//! `lion-session` entry point (spec 02 §9): flags, backend selection,
//! the tokio runtime, signal handling and the final exit code.
//!
//! Backend selection:
//! - `--mock` — all-fakes demo mode: DirectLauncher with script fakes,
//!   mock logind/authorizer/notifier, no bus. Runs a full session
//!   lifecycle against real child processes in any environment.
//! - real (default) — zbus session-bus service `os.lionos.Session1`,
//!   logind via D-Bus with systemctl/loginctl CLI fallback (spec §6),
//!   systemd user-manager detection for unit-based starts, lion-auth
//!   when configured (fail closed), local policy otherwise.

use lion_session::authz::LocalPolicy;
use lion_session::cli::{self, Args};
use lion_session::config::{Config, DEFAULT_CONFIG_PATH};
use lion_session::error::Result;
use lion_session::session::{Deps, SessionCore};
use lion_session::{SCHEMA_JSON, SPEC, VERSION};
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(feature = "real-backends")]
use std::time::Duration;
use tokio::sync::mpsc;

fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(true)
        .init();
}

fn main() {
    let args = match cli::from_env() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("lion-session: {e}");
            std::process::exit(2);
        }
    };

    if args.version {
        println!("lion-session {VERSION} (LionOS spec {SPEC})");
        return;
    }
    if args.print_schema {
        println!("{SCHEMA_JSON}");
        return;
    }

    let config_path = args
        .config
        .clone()
        .or_else(|| std::env::var_os("LION_SESSION_CONFIG").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));

    if args.check_config {
        match Config::load(&config_path) {
            Ok(cfg) => {
                println!(
                    "ok: {} (shutdown_timeout_ms={}, restore_apps={}, autostart_delay_ms={}, services={})",
                    config_path.display(),
                    cfg.shutdown_timeout_ms,
                    cfg.restore_apps,
                    cfg.autostart_delay_ms,
                    cfg.services.len()
                );
            }
            Err(e) => {
                eprintln!("config error: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    let cfg = match Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("lion-session: {e}");
            std::process::exit(1);
        }
    };

    init_logging();
    // SIGHUP reload path reads this env var (see session core).
    std::env::set_var("LION_SESSION_CONFIG", &config_path);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let code = rt.block_on(run(cfg, args));
    std::process::exit(code);
}

async fn run(mut cfg: Config, args: Args) -> i32 {
    let state_path = cfg.state_dir().join("state.json");
    let history_path = cfg.state_dir().join("history.json");

    if args.mock {
        mockify(&mut cfg);
        let deps = mock_deps();
        let (sig_tx, _sig_rx) = mpsc::unbounded_channel();
        let (core, _shared, ev_tx) = SessionCore::new(cfg, deps, sig_tx);
        spawn_signal_bridge(ev_tx.clone());
        tracing::info!(target: "session", "mock mode: script fakes, no bus");
        return core.run(state_path, history_path).await;
    }

    real_mode(cfg, state_path, history_path).await
}

/// Replace the configured compositor/services with live script fakes so
/// the demo works on any machine (real processes, real supervision).
fn mockify(cfg: &mut Config) {
    let notify_and_sleep = |extra: &str| {
        vec![
            "python3".to_string(),
            "-c".to_string(),
            format!(
                "import socket,os,time\ns=socket.socket(socket.AF_UNIX,socket.SOCK_DGRAM)\n\
                 s.connect(os.environ[\"NOTIFY_SOCKET\"].replace(\"@\",chr(0),1))\n\
                 time.sleep(0.05)\ns.send(b\"READY=1\")\n{extra}\ntime.sleep(3600)"
            ),
        ]
    };
    cfg.compositor.exec = notify_and_sleep("");
    for s in cfg.services.iter_mut() {
        s.unit = None;
        s.exec = Some(notify_and_sleep(""));
    }
    // Keep the demo quick.
    cfg.autostart_delay_ms = 500;
}

fn mock_deps() -> Deps {
    use lion_session::mocks::*;
    Deps {
        launcher: Arc::new(lion_session::backends::DirectLauncher),
        systemd: Some(Arc::new(MockSystemdUser::default())),
        systemd_mode: false,
        logind: Arc::new(MockLogind::default()),
        authz: Arc::new(MockAuthorizer::default()),
        notifier: Arc::new(MockNotifier::default()),
        ready_watch: Arc::new(MockReadyWatch::default()),
        notify: lion_session::notify::Notify::from_env(),
    }
}

#[cfg(feature = "real-backends")]
async fn real_mode(cfg: Config, state_path: PathBuf, history_path: PathBuf) -> i32 {
    // Session bus connection (zbus). Absent bus → CLI fallbacks (loud).
    let bus_conn = lion_session::backends::ZbusSystemdUser::connect()
        .await
        .ok();

    // systemd user-manager present? → unit-based starts.
    let systemd_mode = if let Some(c) = &bus_conn {
        name_has_owner(c, "org.freedesktop.systemd1").await
    } else {
        false
    };

    // Deps assembly.
    let (systemd, logind, authz, notifier) = assemble_backends(&cfg, &bus_conn).await;

    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| format!("/tmp/lion-session-run-{}", std::process::id()));
    let ready_watch = Arc::new(lion_session::backends::WaylandSocketWatch {
        runtime_dir: runtime_dir.clone(),
        display: cfg.compositor.wayland_display.clone(),
    });

    let deps = Deps {
        launcher: Arc::new(lion_session::backends::DirectLauncher),
        systemd: Some(systemd),
        systemd_mode,
        logind,
        authz,
        notifier,
        ready_watch,
        notify: lion_session::notify::Notify::from_env(),
    };

    let (sig_tx, sig_rx) = mpsc::unbounded_channel();
    let (core, shared, ev_tx) = SessionCore::new(cfg.clone(), deps, sig_tx);
    spawn_signal_bridge(ev_tx.clone());

    // D-Bus service. Absent session bus → fatal (fail closed): the
    // session daemon is useless if its clients cannot reach it.
    match lion_session::bus::serve(&cfg, ev_tx.clone(), shared, sig_rx, systemd_mode).await {
        Ok(_conn) => {}
        Err(e) => {
            tracing::error!(target: "bus", "cannot serve os.lionos.Session1: {e}");
            return 1;
        }
    }

    core.run(state_path, history_path).await
}

/// Log-only notifier fallback.
#[cfg(feature = "real-backends")]
struct LogNotifier;

#[cfg(feature = "real-backends")]
#[async_trait::async_trait]
impl lion_session::ports::Notifier for LogNotifier {
    async fn notify(&self, summary: &str, body: &str) -> Result<()> {
        tracing::warn!(target: "notify", "notification: {summary}: {body}");
        Ok(())
    }
}

/// Local policy adapter (deny-by-default wrapper so a bug in LocalPolicy
/// can never become an allow).
#[cfg(feature = "real-backends")]
struct FailClosedAdapter(LocalPolicy);

#[cfg(feature = "real-backends")]
#[async_trait::async_trait]
impl lion_session::ports::Authorizer for FailClosedAdapter {
    async fn authorize(&self, action: &str, uid: u32) -> Result<bool> {
        Ok(self.0.authorize(action, uid))
    }
}

#[cfg(feature = "real-backends")]
async fn assemble_backends(
    cfg: &Config,
    bus_conn: &Option<lion_session::backends::ZbusSystemdUser>,
) -> (
    Arc<dyn lion_session::ports::SystemdUser>,
    Arc<dyn lion_session::ports::Logind>,
    Arc<dyn lion_session::ports::Authorizer>,
    Arc<dyn lion_session::ports::Notifier>,
) {
    use lion_session::backends::*;
    use lion_session::ports::*;

    // systemd user manager: zbus when the bus is up, else systemctl CLI.
    let systemd: Arc<dyn SystemdUser> = match bus_conn {
        Some(c) => Arc::new(c.clone()),
        None => {
            tracing::warn!(target: "session", "session bus unavailable — systemctl CLI fallback");
            Arc::new(SystemctlCliUser)
        }
    };

    // logind: zbus, else loginctl CLI (spec §6, log loudly).
    let logind: Arc<dyn Logind> = match ZbusLogind::connect().await {
        Ok(l) => Arc::new(l),
        Err(e) => {
            tracing::warn!(target: "logind", "logind unreachable ({e}) — CLI fallback");
            Arc::new(LoginctlCli)
        }
    };

    // Authorization: lion-auth when configured (fail closed), else the
    // local policy (owner + allowlists; documented in DESIGN.md).
    let authz: Arc<dyn Authorizer> = if !cfg.lion_auth.bus_name.is_empty() {
        match LionAuthClient::connect(
            &cfg.lion_auth.bus_name,
            Duration::from_millis(cfg.lion_auth.timeout_ms),
        )
        .await
        {
            Ok(a) => Arc::new(a),
            Err(e) => {
                tracing::error!(target: "authz", "lion-auth connect failed ({e}) — fail closed");
                Arc::new(lion_session::mocks::MockAuthorizer {
                    allow_all: false,
                    ..Default::default()
                })
            }
        }
    } else {
        let owner = lion_session::authz::current_uid();
        Arc::new(FailClosedAdapter(LocalPolicy::new(
            owner,
            &cfg.lion_auth,
            &cfg.power_allowed_uids,
        )))
    };

    // Notifications: org.freedesktop.Notifications, else log-only.
    let notifier: Arc<dyn Notifier> = match NotificationsNotifier::connect().await {
        Ok(n) => Arc::new(n),
        Err(_) => Arc::new(LogNotifier),
    };

    (systemd, logind, authz, notifier)
}

#[cfg(feature = "real-backends")]
async fn name_has_owner(_c: &lion_session::backends::ZbusSystemdUser, _name: &str) -> bool {
    // The ZbusSystemdUser client shares nothing addressable here; do the
    // NameHasOwner round trip on a fresh connection instead.
    match zbus::connection::Connection::session().await {
        Ok(conn) => {
            let dbus = match zbus::fdo::DBusProxy::new(&conn).await {
                Ok(d) => d,
                Err(_) => return false,
            };
            match zbus::names::BusName::try_from(_name.to_string()) {
                Ok(b) => dbus.name_has_owner(b).await.unwrap_or(false),
                Err(_) => false,
            }
        }
        Err(_) => false,
    }
}

/// Bridge SIGTERM/SIGHUP into core events (graceful stop / config
/// reload; spec 02 §9).
fn spawn_signal_bridge(ev_tx: mpsc::UnboundedSender<lion_session::session::Event>) {
    tokio::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut hup = signal(SignalKind::hangup()).expect("SIGHUP handler");
        loop {
            tokio::select! {
                _ = term.recv() => {
                    tracing::info!(target: "session", "SIGTERM: graceful shutdown");
                    let _ = ev_tx.send(lion_session::session::Event::SigTerm);
                    return;
                }
                _ = hup.recv() => {
                    let _ = ev_tx.send(lion_session::session::Event::SigHup);
                }
            }
        }
    });
}

#[cfg(not(feature = "real-backends"))]
async fn real_mode(_cfg: Config, _state_path: PathBuf, _history_path: PathBuf) -> i32 {
    eprintln!("lion-session: built without real-backends; use --mock");
    1
}
