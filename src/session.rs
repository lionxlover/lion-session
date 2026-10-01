//! Orchestrates one user session end-to-end: bring up the session bus,
//! start the compositor, wait for it to be ready, launch autostart apps,
//! serve `os.lionos.Session`, then tear everything down in reverse order
//! when asked to end.
//!
//! On top of the classic flow this supervisor adds:
//!   - **compositor crash recovery**: a crashed compositor is respawned
//!     (with backoff and a crash-loop guard) instead of ending the
//!     session; supervised autostart apps reconnect on their own -- the
//!     same resilience GNOME gets from auto-restarting gnome-shell, but
//!     for any compositor
//!   - **logind wiring**: suspend locks the session first; system
//!     shutdown ends the session immediately; `loginctl lock-session`
//!     reaches lion-locker; teardown holds a delay inhibitor
//!   - **SIGTERM/SIGINT** end the session gracefully instead of leaking
//!     every child into SIGKILL (what a bare default handler would do)
//!   - **exit codes**: 0 = clean end, 1 = fatal setup error,
//!     3 = compositor crash-loop gave up

use crate::{
    config::{Config, Via},
    env::Paths,
    harden::jitter_ms,
    idle::{IdleAction, IdleMachine},
    inhibit::EndProtocol,
    logind::{Logind, LogindEvent},
    metrics::{Counter, Metrics},
    proc::{self, Launcher},
    service::{EndReason, EndRequest, SessionService},
    sys,
};
use anyhow::{bail, Context, Result};
use std::{process::ExitCode, sync::Arc, time::Duration};
use tokio::{
    net::UnixStream,
    process::Child,
    signal::unix::{signal, SignalKind},
    sync::{mpsc, watch},
    time::{sleep, timeout, Instant},
};
use zbus::{connection, object_server::InterfaceRef, Connection};

pub const SESSION_BUS_NAME: &str = "os.lionos.Session";
pub const SESSION_OBJECT_PATH: &str = "/os/lionos/Session";

/// Exit code when the compositor crash-loop guard trips.
pub const EXIT_COMPOSITOR_CRASHLOOP: u8 = 3;

/// How long autostart apps get to exit on their own after the shutdown
/// flag flips before we start signalling process groups.
const APP_DRAIN_MS: u64 = 150;

/// Idle escalation tick: 1 Hz is plenty for minute-scale policies and
/// costs nothing.
const IDLE_TICK: Duration = Duration::from_secs(1);

/// Restart backoff jitter bound for the compositor (see `harden::jitter_ms`).
const COMPOSITOR_JITTER_MS: u64 = 150;

pub async fn run(cfg: Config, paths: Paths) -> Result<ExitCode> {
    let mut session_bus = crate::bus::ensure(&paths.runtime_dir)
        .await
        .context("setting up session D-Bus bus")?;

    // 0.3.0 startup ordering: everything the *service surface* needs
    // (bus, one peer connection) happens first, and the name is
    // acquired before the compositor even spawns. The systemd probe,
    // activation-env import and logind subscribe — subprocess- and
    // bus-heavy — move behind name acquisition, onto the path that
    // only matters before apps start (after the compositor is ready).
    // Shells and the greeter see `os.lionos.Session` a full
    // compositor-spawn earlier than 0.2.0 (measured in TEST_REPORT).

    let metrics = std::sync::Arc::new(Metrics::new());
    let (end_tx, mut end_rx) = mpsc::unbounded_channel::<EndRequest>();
    let protocol = std::sync::Arc::new(EndProtocol::new(Duration::from_millis(
        cfg.session.end_timeout_ms,
    )));

    // A separate connection just for forwarding Lock/Restart/Shutdown to
    // lion-locker / lion-power, so a slow or wedged peer can never block
    // the service's own incoming calls. If even this fails, we still serve
    // the session (Lock/Restart/Shutdown will log and no-op) rather than
    // refuse to give the user a desktop at all.
    let peer_bus = match Connection::session().await {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::error!(error = %e, "no D-Bus connection available for forwarding calls");
            None
        }
    };

    let logind = Logind::connect().await;
    let mut logind_rx = match &logind {
        Some(l) => l.events().await.ok(),
        None => None,
    };

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut app_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    let mut iface: Option<InterfaceRef<SessionService>> = None;
    // The serving connection's *lifetime* is the whole session; after
    // construction it is never read again (the object server holds the
    // interface), so the binding is underscore-named on purpose.
    let mut _service_conn: Option<Connection> = None;

    // The idle escalation machine is shared between the SetIdle D-Bus
    // method and the tick task below; take the handle before the
    // service object moves into the object server.
    let mut idle_machine: Option<Arc<std::sync::Mutex<IdleMachine>>> = None;
    let idle_configured = cfg.session.idle.is_configured();

    if let Some(peer_bus) = peer_bus.as_ref() {
        let service = SessionService::new(
            cfg.services.clone(),
            cfg.session.lock_on_sleep,
            cfg.session.lock_on_shutdown,
            cfg.session.idle,
            peer_bus.clone(),
            end_tx.clone(),
            protocol.clone(),
            metrics.clone(),
            logind.clone(),
        );
        idle_machine = Some(service.idle_machine());
        match build_service(service).await {
            Ok(conn) => {
                if let Ok(r) = crate::service::interface_ref(&conn, SESSION_OBJECT_PATH).await {
                    r.get().await.attach(&conn, SESSION_OBJECT_PATH).await;
                    iface = Some(r);
                }
                tracing::info!(
                    bus = SESSION_BUS_NAME,
                    path = SESSION_OBJECT_PATH,
                    "IPC ready"
                );
                _service_conn = Some(conn);
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to serve {SESSION_BUS_NAME}");
            }
        }
    }

    // 0.3.0: idle escalation tick — 1 Hz while a policy is configured,
    // not spawned at all otherwise (0.2.0 behaviour, zero overhead).
    if idle_configured {
        if let (Some(machine), Some(iface_ref)) = (idle_machine, iface.as_ref()) {
            let iface_ref = iface_ref.clone();
            let end_tx_idle = end_tx.clone();
            tokio::spawn(async move {
                let machine = machine;
                loop {
                    tokio::time::sleep(IDLE_TICK).await;
                    let action = machine.lock().unwrap().poll();
                    match action {
                        IdleAction::Lock => {
                            iface_ref.get().await.escalate_lock().await;
                        }
                        IdleAction::EndSession => {
                            let _ = end_tx_idle.send(EndRequest {
                                reason: EndReason::Logout,
                                inhibitor: None,
                                fast: false,
                            });
                        }
                        IdleAction::None => {}
                    }
                }
            });
        }
    }

    // Subprocess- and probe-heavy steps (0.3.0: after name acquisition):
    // the activation-env import only matters to D-Bus-activated services
    // and `systemd --user`, both consulted after the compositor is up.
    sys::import_activation_env().await;
    let systemd_ok = sys::systemd_user_available().await;
    let launcher = match cfg.session.via {
        Via::Never => Launcher::Direct,
        Via::Auto if systemd_ok => {
            tracing::info!("systemd --user reachable: autostart apps will get their own scopes");
            Launcher::SystemdScope
        }
        Via::Auto => {
            tracing::info!("no systemd --user reachable: autostart apps as direct children");
            Launcher::Direct
        }
    };

    let mut compositor: Option<Child>;
    let mut restarts: Vec<Instant> = Vec::new();
    let mut exit_code = ExitCode::SUCCESS;
    let mut first_start = true;

    'outer: loop {
        compositor = Some(spawn_compositor(&cfg, &paths).await?);
        tracing::info!("compositor ready");

        if first_start {
            // Autostart: configured apps + XDG desktop entries, ordered.
            let mut apps = if cfg.session.xdg_autostart {
                crate::desktop::load_xdg_autostart(cfg.session.supervise_xdg)
            } else {
                Vec::new()
            };
            apps.extend(cfg.enabled_apps_sorted());
            for app in apps {
                app_handles.push(tokio::spawn(proc::run_app(
                    app,
                    shutdown_rx.clone(),
                    launcher,
                    metrics.clone(),
                )));
            }

            let _ = crate::sdnotify::spawn_watchdog();
            crate::sdnotify::status("session running");
        } else if let Some(r) = iface.as_ref() {
            // Compositor respawn after a crash: tell the shell.
            r.get()
                .await
                .emit_compositor_restarted(restarts.len() as u32)
                .await;
        }
        first_start = false;

        // Even without a working IPC surface, run the session: it can
        // still be ended by the compositor exiting or a signal, and a
        // broken D-Bus setup shouldn't strand the user with no way out
        // short of a hard reset.
        let mut sig_term = signal(SignalKind::terminate())?;
        let mut sig_int = signal(SignalKind::interrupt())?;

        let outcome = loop {
            tokio::select! {
                biased;
                req = end_rx.recv() => match req {
                    Some(req) => break Outcome::End(req),
                    None => break Outcome::Signal(EndReason::Logout),
                },
                status = compositor.as_mut().expect("compositor spawned").wait() => {
                    tracing::warn!(?status, "compositor exited");
                    break Outcome::CompositorExit;
                }
                ev = async {
                    match logind_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending::<Option<LogindEvent>>().await,
                    }
                } => {
                    if let Some(ev) = ev {
                        handle_logind_event(ev, iface.as_ref()).await;
                    }
                    continue;
                }
                _ = sig_term.recv() => break Outcome::Signal(EndReason::Logout),
                _ = sig_int.recv() => break Outcome::Signal(EndReason::Logout),
            }
        };

        match outcome {
            Outcome::End(req) => {
                teardown(
                    req,
                    &mut compositor,
                    &mut app_handles,
                    &shutdown_tx,
                    &mut session_bus,
                    cfg.logout_animation_ms,
                )
                .await;
                break 'outer;
            }
            Outcome::Signal(reason) => {
                tracing::info!(?reason, "termination signal received, ending session");
                match iface.as_ref() {
                    Some(r) => r.get().await.force_end(reason).await,
                    None => {
                        let _ = end_tx.send(EndRequest {
                            reason,
                            inhibitor: None,
                            fast: true,
                        });
                    }
                }
                // The end request lands in end_rx; consume it and tear down.
                if let Some(req) = end_rx.recv().await {
                    teardown(
                        req,
                        &mut compositor,
                        &mut app_handles,
                        &shutdown_tx,
                        &mut session_bus,
                        cfg.logout_animation_ms,
                    )
                    .await;
                }
                break 'outer;
            }
            Outcome::CompositorExit => {
                if !cfg.compositor.restart {
                    tracing::error!("compositor restart disabled, ending session");
                    let _ = end_tx.send(EndRequest {
                        reason: EndReason::Logout,
                        inhibitor: None,
                        fast: true,
                    });
                    if let Some(req) = end_rx.recv().await {
                        teardown(
                            req,
                            &mut compositor,
                            &mut app_handles,
                            &shutdown_tx,
                            &mut session_bus,
                            cfg.logout_animation_ms,
                        )
                        .await;
                    }
                    exit_code = ExitCode::from(EXIT_COMPOSITOR_CRASHLOOP);
                    break 'outer;
                }
                let now = Instant::now();
                restarts.retain(|t| {
                    now.duration_since(*t) < Duration::from_millis(cfg.compositor.crash_window_ms)
                });
                if restarts.len() >= cfg.compositor.max_restarts as usize {
                    metrics.inc(Counter::CompositorCrashloop);
                    tracing::error!(
                        restarts = restarts.len(),
                        "compositor crash-looping, ending session"
                    );
                    let _ = end_tx.send(EndRequest {
                        reason: EndReason::Logout,
                        inhibitor: None,
                        fast: true,
                    });
                    if let Some(req) = end_rx.recv().await {
                        teardown(
                            req,
                            &mut compositor,
                            &mut app_handles,
                            &shutdown_tx,
                            &mut session_bus,
                            cfg.logout_animation_ms,
                        )
                        .await;
                    }
                    exit_code = ExitCode::from(EXIT_COMPOSITOR_CRASHLOOP);
                    break 'outer;
                }
                restarts.push(now);
                metrics.inc(Counter::CompositorRestarts);
                let backoff = Duration::from_millis(
                    300 * restarts.len() as u64 + jitter_ms(COMPOSITOR_JITTER_MS),
                );
                tracing::warn!(attempt = restarts.len(), backoff = ?backoff, "restarting compositor");
                sleep(backoff).await;
                // loop continues -> fresh spawn_compositor
            }
        }
    }

    crate::sdnotify::stopping();
    Ok(exit_code)
}

enum Outcome {
    End(EndRequest),
    CompositorExit,
    Signal(EndReason),
}

async fn handle_logind_event(ev: LogindEvent, iface: Option<&InterfaceRef<SessionService>>) {
    match ev {
        LogindEvent::PrepareShutdown => {
            tracing::info!("logind is about to shut the system down, ending fast");
            if let Some(r) = iface {
                // Lock first (0.3.0): a cancelled shutdown must leave a
                // locked session, not an exposed one.
                r.get().await.shutdown_imminent().await;
                r.get().await.force_end(EndReason::Shutdown).await;
            }
        }
        LogindEvent::PrepareSleep(start) => {
            if let Some(r) = iface {
                r.get().await.handle_sleep(start).await;
            }
        }
        LogindEvent::SessionLock(lock) => {
            if let Some(r) = iface {
                r.get().await.forward_lock(lock).await;
            }
        }
    }
}

/// Reverse-order teardown: apps -> compositor -> user target -> session
/// bus, then release the logind delay inhibitor (if one was held) by
/// dropping its fd.
async fn teardown(
    req: EndRequest,
    compositor: &mut Option<Child>,
    app_handles: &mut Vec<tokio::task::JoinHandle<()>>,
    shutdown_tx: &watch::Sender<bool>,
    session_bus: &mut crate::bus::SessionBus,
    logout_animation_ms: u64,
) {
    tracing::info!(
        reason = req.reason.as_str(),
        fast = req.fast,
        "ending session"
    );
    if !req.fast && logout_animation_ms > 0 {
        // The shell heard PreparingToEnd and is playing its fade-out.
        sleep(Duration::from_millis(logout_animation_ms)).await;
    }

    let _ = shutdown_tx.send(true);
    // Give apps a beat to observe the shutdown flag and exit by
    // themselves before we start signalling process groups.
    sleep(Duration::from_millis(APP_DRAIN_MS)).await;
    for h in app_handles.drain(..) {
        // A wedged app task must never hang teardown; 2 s each, then
        // move on (the children still get kill_on_drop).
        let _ = timeout(Duration::from_secs(2), h).await;
    }
    if let Some(c) = compositor.as_mut() {
        proc::terminate(c, proc::GRACE).await;
    }
    sys::user_target("stop").await;
    session_bus.shutdown().await;
    // Releasing the logind delay inhibitor is just dropping the fd.
    drop(req.inhibitor);
    tracing::info!("session torn down");
}

async fn build_service(service: SessionService) -> zbus::Result<Connection> {
    connection::Builder::session()?
        .name(SESSION_BUS_NAME)?
        .serve_at(SESSION_OBJECT_PATH, service)?
        .build()
        .await
}

/// Start the compositor and wait for its Wayland socket to appear, so
/// nothing races the compositor's own startup. A socket left behind by a
/// crashed previous compositor is detected and removed; a socket that is
/// still live (another session's compositor) is refused loudly rather
/// than silently stolen.
async fn spawn_compositor(cfg: &Config, paths: &Paths) -> Result<Child> {
    if paths.wayland_socket.exists() {
        match UnixStream::connect(&paths.wayland_socket).await {
            Ok(_) => bail!(
                "wayland socket {} is already served by a live compositor; \
                 refusing to steal it (two sessions on one socket?)",
                paths.wayland_socket.display()
            ),
            Err(_) => {
                tracing::debug!("removing stale wayland socket from a crashed compositor");
                let _ = std::fs::remove_file(&paths.wayland_socket);
            }
        }
    }

    let mut child = proc::spawn(&cfg.compositor.command, &cfg.compositor.args)
        .with_context(|| format!("failed to exec {}", cfg.compositor.command))?;

    let deadline = Duration::from_millis(cfg.compositor.ready_timeout_ms);
    let started = Instant::now();
    let ready = timeout(deadline, async {
        while !paths.wayland_socket.exists() {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await;

    if ready.is_err() {
        // Never-ready compositor: take it down (kill_on_drop also covers
        // this) and refuse to start the session on a dead display.
        let _ = child.wait().await;
        bail!(
            "compositor did not create {} within {:?}",
            paths.wayland_socket.display(),
            deadline
        );
    }
    tracing::debug!(elapsed = ?started.elapsed(), "compositor socket appeared");
    Ok(child)
}
