#![forbid(unsafe_code)]
//! The session core: startup orchestration, supervision and lifecycle
//! (spec 02 §3). One task owns all mutable state; everything else —
//! bus methods, child exits, notify frames, inhibitor EOFs, timers —
//! arrives as [`Event`]s on a single channel. That keeps ordering
//! deterministic (tests drive the core directly without a bus) and the
//! daemon idle: every wait is `select!`-ed, no polling, no wakeups with
//! nothing to do (spec 02 §7).
//!
//! Startup: history → safe-mode decision → environment → import into
//! systemd --user → compositor (await READY=1) → shell services in
//! resolver levels → ready gates → SessionReady → restored apps and
//! XDG autostart (after `autostart_delay_ms`).

use crate::authz::actions;
use crate::autostart::{AutostartEntry, PathTryExecChecker};
use crate::config::{Config, RestartPolicy, ServiceSpec};
use crate::environment::{EnvInputs, SessionEnv};
use crate::error::{Error, Result};
use crate::inhibitors::{self, InhibitorStore};
use crate::lifecycle::{Action, InhibitorSnapshot, Machine, Output};
use crate::notify::Notify;
use crate::ports::{
    Authorizer, ChildProcess, ExitInfo, Launcher, Logind, Notifier, ReadyWatch, Spawned,
    SystemdUser,
};
use crate::resolver::{self, Plan};
use crate::safemode::{self, History};
use crate::savestate::SessionState;
use crate::supervisor::{self, Decision, SupState};
use crate::throttle::RateLimiter;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch};

/// Signals the core asks the bus layer to emit.
#[derive(Debug, Clone)]
pub enum SignalReq {
    SessionReady,
    QueryEndSession(u32),
    EndSession(u32),
    ServiceFailed(String, String),
}

/// Read-only projections for D-Bus properties (watch channels: the bus
/// layer re-emits PropertiesChanged on change).
#[derive(Clone)]
pub struct Shared {
    pub state: watch::Receiver<String>,
    pub inhibited_actions: watch::Receiver<Vec<String>>,
    pub safe_mode: watch::Receiver<bool>,
    /// Which clients/inhibitors are holding up the end-session query.
    pub blockers: watch::Receiver<Vec<String>>,
}

struct SharedTx {
    state: watch::Sender<String>,
    inhibited: watch::Sender<Vec<String>>,
    safe_mode: watch::Sender<bool>,
    blockers: watch::Sender<Vec<String>>,
}

impl SharedTx {
    fn channels() -> (SharedTx, Shared) {
        let (stx, srx) = watch::channel("starting".to_string());
        let (itx, irx) = watch::channel(Vec::<String>::new());
        let (xtx, xrx) = watch::channel(false);
        let (btx, brx) = watch::channel(Vec::<String>::new());
        (
            SharedTx {
                state: stx,
                inhibited: itx,
                safe_mode: xtx,
                blockers: btx,
            },
            Shared {
                state: srx,
                inhibited_actions: irx,
                safe_mode: xrx,
                blockers: brx,
            },
        )
    }
    fn set_state(&self, v: &str) {
        let _ = self.state.send(v.to_string());
    }
    fn set_inhibited(&self, v: Vec<String>) {
        let _ = self.inhibited.send(v);
    }
    fn set_safe_mode(&self, v: bool) {
        let _ = self.safe_mode.send(v);
    }
    fn set_blockers(&self, v: Vec<String>) {
        let _ = self.blockers.send(v);
    }
}

/// Everything the core drives. `systemd_mode` selects unit-based starts
/// (org.freedesktop.systemd1) over direct child spawning; the systemd
/// port, when present, always receives the environment import.
pub struct Deps {
    pub launcher: Arc<dyn Launcher>,
    pub systemd: Option<Arc<dyn SystemdUser>>,
    pub systemd_mode: bool,
    pub logind: Arc<dyn Logind>,
    pub authz: Arc<dyn Authorizer>,
    pub notifier: Arc<dyn Notifier>,
    pub ready_watch: Arc<dyn ReadyWatch>,
    pub notify: Notify,
}

/// Inbound events (bus layer, child tasks, signals, timers).
pub enum Event {
    ChildExit {
        name: String,
        info: ExitInfo,
    },
    ChildReady {
        name: String,
    },
    Respawn {
        name: String,
    },
    CompositorReady,
    /// systemd JobRemoved for a supervised unit (result "done"/"failed").
    ServiceJobDone {
        unit: String,
        result: String,
    },
    QueryDeadline,
    AutostartGo {
        entry: AutostartEntry,
        restored: bool,
    },
    InhibitReq {
        what: String,
        who: String,
        why: String,
        owner: String,
        uid: u32,
        reply: oneshot::Sender<std::result::Result<std::os::unix::net::UnixStream, String>>,
    },
    InhibitorReleased {
        id: u64,
    },
    RegisterClient {
        app_id: String,
        owner: String,
        uid: u32,
        pid: u32,
        reply: oneshot::Sender<Result<()>>,
    },
    EndSessionReply {
        app_id: String,
        reply: oneshot::Sender<Result<()>>,
    },
    ClientGone {
        owner: String,
    },
    Lifecycle {
        action: Action,
        uid: u32,
        reply: oneshot::Sender<Result<()>>,
    },
    Lock {
        uid: u32,
        reply: oneshot::Sender<Result<()>>,
    },
    SwitchUser {
        uid: u32,
        reply: oneshot::Sender<Result<()>>,
    },
    SigTerm,
    SigHup,
}

struct ClientInfo {
    #[allow(dead_code)]
    app_id: String,
    #[allow(dead_code)]
    uid: u32,
    #[allow(dead_code)]
    pid: u32,
}

struct ServiceRun {
    spec: ServiceSpec,
    child: Option<Arc<dyn ChildProcess>>,
    sup: SupState,
    ready: bool,
}

/// The running core. `run()` consumes it and returns the exit code.
pub struct SessionCore {
    cfg: Config,
    deps: Deps,
    ev_tx: mpsc::UnboundedSender<Event>,
    ev_rx: mpsc::UnboundedReceiver<Event>,
    sig_tx: mpsc::UnboundedSender<SignalReq>,
    shared_tx: SharedTx,
    env: Option<SessionEnv>,
    machine: Machine,
    inhibitors: InhibitorStore,
    clients: BTreeMap<String, ClientInfo>,
    owner_by_name: HashMap<String, Vec<String>>,
    services: HashMap<String, ServiceRun>,
    compositor: Option<ServiceRun>,
    autostarted: BTreeMap<String, Vec<String>>,
    plan: Option<Plan>,
    inhibit_limit: RateLimiter,
    register_limit: RateLimiter,
    session_ready: bool,
    ending: bool,
    history: History,
    safe_mode: bool,
    bad_start_recorded: bool,
    started_at: Instant,
    t_startup: Option<Duration>,
    /// Events observed while awaiting compositor/gate readiness.
    buffered: Vec<Event>,
}

impl SessionCore {
    pub fn new(
        cfg: Config,
        deps: Deps,
        sig_tx: mpsc::UnboundedSender<SignalReq>,
    ) -> (SessionCore, Shared, mpsc::UnboundedSender<Event>) {
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let (shared_tx, shared) = SharedTx::channels();
        let core = SessionCore {
            inhibit_limit: RateLimiter::new(Duration::from_secs(60), cfg.inhibit.rate_per_minute),
            register_limit: RateLimiter::new(
                Duration::from_secs(60),
                cfg.bus.register_rate_per_minute,
            ),
            machine: Machine::new(Duration::from_millis(cfg.shutdown_timeout_ms)),
            cfg,
            deps,
            ev_tx: ev_tx.clone(),
            ev_rx,
            sig_tx,
            shared_tx,
            env: None,
            inhibitors: InhibitorStore::default(),
            clients: BTreeMap::new(),
            owner_by_name: HashMap::new(),
            services: HashMap::new(),
            compositor: None,
            autostarted: BTreeMap::new(),
            plan: None,
            session_ready: false,
            ending: false,
            history: History::default(),
            safe_mode: false,
            bad_start_recorded: false,
            started_at: Instant::now(),
            t_startup: None,
            buffered: Vec::new(),
        };
        (core, shared, ev_tx)
    }

    /// Run the session to completion; returns the daemon exit code.
    pub async fn run(mut self, state_path: PathBuf, history_path: PathBuf) -> i32 {
        let t0 = self.started_at;
        if let Err(e) = self.startup(&state_path, &history_path).await {
            tracing::error!(target: "session", "startup failed: {e}");
            self.deps.notify.stopping();
            if !self.bad_start_recorded {
                self.history.record_bad_start();
                self.bad_start_recorded = true;
            }
            let _ = self.history.save(&history_path);
            return 1;
        }
        self.t_startup = Some(t0.elapsed());
        self.session_ready = true;
        self.shared_tx.set_state("running");
        let _ = self.sig_tx.send(SignalReq::SessionReady);
        self.deps.notify.ready();
        self.history.record_ready();
        let _ = self.history.save(&history_path);
        tracing::info!(
            target: "session",
            startup_us = t0.elapsed().as_micros() as u64,
            "SessionReady"
        );

        // ── drain startup-buffered events ─────────────────────────────
        // Events that arrived while we awaited compositor/gate readiness
        // (client registrations, inhibit requests…) must be processed now:
        // without a new event the loop below would never look at them.
        let mut ended_during_drain = false;
        for ev in std::mem::take(&mut self.buffered) {
            if self.handle_one(ev, &state_path, &history_path).await {
                ended_during_drain = true;
                break;
            }
            self.sync_blockers();
        }

        // ── event loop ────────────────────────────────────────────────
        if !ended_during_drain {
            let watchdog = self.deps.notify.watchdog_interval();
            let watchdog_on = watchdog.is_some();
            let mut watchdog_tick =
                tokio::time::interval(watchdog.unwrap_or(Duration::from_secs(u64::MAX / 2)));
            watchdog_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    ev = self.ev_rx.recv() => {
                        let Some(ev) = ev else { break };
                        if self.handle_one(ev, &state_path, &history_path).await {
                            break;
                        }
                        self.sync_blockers();
                    }
                    _ = watchdog_tick.tick(), if watchdog_on => {
                        self.deps.notify.watchdog_tick();
                    }
                }
            }
        }

        // ── teardown ──────────────────────────────────────────────────
        self.shared_tx.set_state("ended");
        self.deps.notify.stopping();
        if !self.bad_start_recorded {
            self.history.record_clean_end();
        }
        let _ = self.history.save(&history_path);
        0
    }

    // ── startup ─────────────────────────────────────────────────────

    async fn startup(
        &mut self,
        state_path: &std::path::Path,
        history_path: &std::path::Path,
    ) -> Result<()> {
        self.history = History::load(history_path);
        let decision = safemode::decide(&self.history, self.cfg.safe_mode.threshold);
        self.safe_mode = decision.active;
        self.shared_tx.set_safe_mode(decision.active);
        if decision.active {
            tracing::warn!(target: "safemode", "safe mode: {}", decision.reason);
            let _ = self
                .deps
                .notifier
                .notify("LionOS safe mode", &decision.reason)
                .await;
        }

        // 1. environment
        let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
            .unwrap_or_else(|_| format!("/tmp/lion-session-run-{}", std::process::id()));
        let source: BTreeMap<String, String> = std::env::vars().collect();
        let imports = crate::environment::import_from_env(&source, &self.cfg.environment.import);
        let inputs = EnvInputs {
            imports,
            xdg_runtime_dir: runtime_dir,
            wayland_display: Some(self.cfg.compositor.wayland_display.clone()),
            desktop_name: self.cfg.desktop_name.clone(),
            dbus_session_bus_address: std::env::var("DBUS_SESSION_BUS_ADDRESS").ok(),
            theme: BTreeMap::new(),
            extra: self.cfg.environment.extra.clone(),
        };
        let env = SessionEnv::build(&inputs).map_err(Error::Config)?;

        // 2. import into systemd --user
        if let Some(sd) = &self.deps.systemd {
            let list = env.systemd_import_list();
            if let Err(e) = sd.set_environment(&list).await {
                tracing::warn!(target: "session", "systemd import-environment failed: {e}");
            }
        }
        self.env = Some(env);

        // 3. plan (safe mode prunes to the minimal set)
        let specs: Vec<ServiceSpec> = if self.safe_mode {
            self.cfg.safe_mode_services()
        } else {
            self.cfg.services.clone()
        };
        let keep: HashSet<String> = specs.iter().map(|s| s.name.clone()).collect();
        let plan = resolver::resolve_subset(&self.cfg.services, &keep).map_err(Error::Config)?;
        self.plan = Some(plan.clone());

        // 4. compositor
        self.start_compositor().await?;

        // 5. await compositor readiness (READY=1 frame; buffered events
        //    queue everything else)
        let timeout = Duration::from_millis(self.cfg.startup.compositor_ready_timeout_ms);
        if !self.wait_compositor_ready(timeout).await? {
            self.history.record_bad_start();
            self.bad_start_recorded = true;
            let _ = self.history.save(history_path);
            return Err(Error::Service(
                "compositor did not become ready in time".into(),
            ));
        }

        // 6. shell services in plan order
        self.start_services(&plan, &specs).await?;

        // 7. ready gates (panel + wallpaper) or timeout (non-fatal)
        let gates: Vec<String> = specs
            .iter()
            .filter(|s| s.ready_gate)
            .map(|s| s.name.clone())
            .collect();
        if !gates.is_empty() {
            let gate_timeout = Duration::from_millis(self.cfg.startup.shell_ready_timeout_ms);
            if !self.wait_gates(&gates, gate_timeout).await {
                tracing::warn!(target: "session", "shell ready gates timed out; continuing");
            }
        }

        // 8. restore + XDG autostart (after shell ready)
        self.schedule_autostart(state_path).await;
        Ok(())
    }

    async fn start_compositor(&mut self) -> Result<()> {
        let comp = self.cfg.compositor.clone();
        let spec = ServiceSpec {
            name: "compositor".into(),
            unit: comp.unit.clone(),
            exec: Some(comp.exec.clone()),
            after: vec![],
            restart: RestartPolicy::Never,
            ready_gate: false,
        };
        if let (true, Some(sd), Some(unit)) =
            (self.deps.systemd_mode, &self.deps.systemd, &comp.unit)
        {
            if let Err(e) = sd.start_unit(unit, "fail").await {
                tracing::warn!(target: "session", "start {unit} failed: {e}");
            }
            self.compositor = Some(ServiceRun {
                spec,
                child: None,
                sup: SupState::default(),
                ready: false,
            });
            return Ok(());
        }
        let env = self.env.as_ref().expect("env built").child_env();
        let spawned = self
            .deps
            .launcher
            .spawn("compositor", &comp.exec, &env)
            .await?;
        let run = self.attach_tasks("compositor", spawned);
        self.compositor = Some(ServiceRun { spec, ..run });
        Ok(())
    }

    /// Spawn the wait + notify tasks for a child; returns the ServiceRun
    /// shell (caller supplies `spec` via struct update).
    fn attach_tasks(&mut self, name: &str, spawned: Spawned) -> ServiceRun {
        let Spawned { child, notify } = spawned;
        let child: Arc<dyn ChildProcess> = child.into();
        let pid = child.pid();
        let ev_tx = self.ev_tx.clone();
        let wait_child = child.clone();
        let wname = name.to_string();
        tokio::spawn(async move {
            let info = match wait_child.wait().await {
                Ok(i) => i,
                Err(e) => {
                    tracing::warn!(target: "session", "wait {wname}: {e}");
                    ExitInfo {
                        code: None,
                        abnormal: true,
                    }
                }
            };
            let _ = ev_tx.send(Event::ChildExit { name: wname, info });
        });
        if let Some(sock) = notify {
            let ev_tx = self.ev_tx.clone();
            let nname = name.to_string();
            tokio::spawn(async move {
                let mut buf = [0u8; 256];
                // "READY=1" is 7 bytes — windows(7), not 8 (a slice==array
                // compare of different lengths silently compiles to false).
                while let Ok((n, _)) = sock.recv_from(&mut buf).await {
                    if buf[..n].windows(7).any(|w| w == b"READY=1") {
                        let ev = if nname == "compositor" {
                            Event::CompositorReady
                        } else {
                            Event::ChildReady {
                                name: nname.clone(),
                            }
                        };
                        let _ = ev_tx.send(ev);
                    }
                }
            });
        }
        tracing::info!(target: "spawn", name, pid, "child started");
        ServiceRun {
            spec: ServiceSpec {
                name: name.into(),
                unit: None,
                exec: None,
                after: vec![],
                restart: RestartPolicy::Always,
                ready_gate: false,
            },
            child: Some(child),
            sup: SupState::default(),
            ready: false,
        }
    }

    async fn wait_compositor_ready(&mut self, timeout: Duration) -> Result<bool> {
        if let Some(c) = &self.compositor {
            if c.ready {
                return Ok(true);
            }
        }
        let deadline = Instant::now() + timeout;
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                // last resort: the port-based watch (socket file)
                let _ = self
                    .deps
                    .ready_watch
                    .wait_ready(Duration::from_millis(200))
                    .await;
                return Ok(self.compositor.as_ref().map(|c| c.ready).unwrap_or(false));
            };
            tokio::select! {
                ev = self.ev_rx.recv() => {
                    let Some(ev) = ev else { return Ok(false) };
                    match ev {
                        Event::CompositorReady => {
                            if let Some(c) = self.compositor.as_mut() {
                                c.ready = true;
                            }
                            return Ok(true);
                        }
                        Event::ChildReady { name } if name == "compositor" => {
                            if let Some(c) = self.compositor.as_mut() {
                                c.ready = true;
                            }
                            return Ok(true);
                        }
                        Event::ChildExit { name, info } if name == "compositor" => {
                            return Err(Error::Service(format!(
                                "compositor exited before ready (code {:?}, abnormal={})",
                                info.code, info.abnormal
                            )));
                        }
                        other => self.buffered.push(other),
                    }
                }
                _ = tokio::time::sleep(remaining) => {
                    return Ok(self.compositor.as_ref().map(|c| c.ready).unwrap_or(false));
                }
            }
        }
    }

    async fn start_services(&mut self, plan: &Plan, specs: &[ServiceSpec]) -> Result<()> {
        let by_name: HashMap<&str, &ServiceSpec> =
            specs.iter().map(|s| (s.name.as_str(), s)).collect();
        for level in &plan.levels {
            for name in level {
                if let Some(spec) = by_name.get(name.as_str()) {
                    self.spawn_service(spec).await?;
                }
            }
        }
        Ok(())
    }

    async fn spawn_service(&mut self, spec: &ServiceSpec) -> Result<()> {
        if self.ending {
            return Ok(());
        }
        if let (true, Some(sd), Some(unit)) =
            (self.deps.systemd_mode, &self.deps.systemd, &spec.unit)
        {
            if let Err(e) = sd.start_unit(unit, "fail").await {
                tracing::warn!(target: "session", "start {unit} failed: {e}");
            }
            self.services.insert(
                spec.name.clone(),
                ServiceRun {
                    spec: spec.clone(),
                    child: None,
                    sup: SupState::default(),
                    ready: false,
                },
            );
            return Ok(());
        }
        let Some(argv) = spec.exec.clone() else {
            // unit-only service in direct mode: nothing to spawn
            return Ok(());
        };
        let env = self.env.as_ref().expect("env built").child_env();
        match self.deps.launcher.spawn(&spec.name, &argv, &env).await {
            Ok(spawned) => {
                // The supervision state (failure/backoff windows) must
                // SURVIVE respawns — a fresh SupState here would reset the
                // crash-loop counter on every restart.
                let prior_sup = self
                    .services
                    .remove(&spec.name)
                    .map(|r| r.sup)
                    .unwrap_or_default();
                let run = self.attach_tasks(&spec.name, spawned);
                self.services.insert(
                    spec.name.clone(),
                    ServiceRun {
                        spec: spec.clone(),
                        sup: prior_sup,
                        ..run
                    },
                );
                Ok(())
            }
            Err(e) => {
                // Spawn failure counts as a crash for supervision (same
                // persistent state rule).
                tracing::warn!(target: "session", "spawn {} failed: {e}", spec.name);
                let mut st = self
                    .services
                    .remove(&spec.name)
                    .map(|r| r.sup)
                    .unwrap_or_default();
                let now = Instant::now();
                let d =
                    supervisor::on_exit(&mut st, spec.restart, &self.cfg.crash_loop, now, false);
                self.services.insert(
                    spec.name.clone(),
                    ServiceRun {
                        spec: spec.clone(),
                        child: None,
                        sup: st,
                        ready: false,
                    },
                );
                self.apply_decision(&spec.name, d);
                Ok(())
            }
        }
    }

    async fn wait_gates(&mut self, gates: &[String], timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if gates.iter().all(|g| {
                self.services
                    .get(g)
                    .map(|s| s.ready)
                    .unwrap_or(self.gate_seen(g))
            }) {
                return true;
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            tokio::select! {
                ev = self.ev_rx.recv() => {
                    let Some(ev) = ev else { return false };
                    match ev {
                        Event::ChildReady { name } => {
                            if let Some(s) = self.services.get_mut(&name) {
                                s.ready = true;
                            } else if name == "compositor" {
                                if let Some(c) = self.compositor.as_mut() {
                                    c.ready = true;
                                }
                            } else if !self.gate_unknown(&name) {
                                // service not yet registered (spawn task raced);
                                // remember it so the gate check can see it.
                                self.buffered.push(Event::ChildReady { name });
                            }
                        }
                        Event::CompositorReady => {
                            if let Some(c) = self.compositor.as_mut() {
                                c.ready = true;
                            }
                        }
                        other => self.buffered.push(other),
                    }
                }
                _ = tokio::time::sleep(remaining) => return false,
            }
        }
    }

    fn gate_seen(&self, g: &str) -> bool {
        self.buffered
            .iter()
            .any(|e| matches!(e, Event::ChildReady { name } if name == g))
    }

    fn gate_unknown(&self, name: &str) -> bool {
        self.buffered
            .iter()
            .any(|e| matches!(e, Event::ChildReady { name: n } if n == name))
    }

    async fn schedule_autostart(&mut self, state_path: &std::path::Path) {
        if self.cfg.restore_apps {
            if let Some(state) = SessionState::load(state_path) {
                let n = state.apps.len();
                for app in state.apps {
                    let entry = AutostartEntry {
                        id: format!("restore-{}", app.app_id),
                        exec: app.exec,
                        delay: Duration::ZERO,
                    };
                    self.spawn_autostart_task(entry, true);
                }
                tracing::info!(target: "session", "scheduling {n} restored app(s)");
            }
        }
        if self.safe_mode {
            tracing::info!(target: "session", "safe mode: XDG autostart skipped");
            return;
        }
        let dirs = self.cfg.autostart_dirs_expanded();
        let entries =
            crate::autostart::scan_dirs(&dirs, &self.cfg.desktop_name, &PathTryExecChecker);
        tracing::info!(target: "session", "autostart: {} entries scheduled", entries.len());
        for entry in entries {
            self.spawn_autostart_task(entry, false);
        }
    }

    fn spawn_autostart_task(&self, entry: AutostartEntry, restored: bool) {
        let ev_tx = self.ev_tx.clone();
        let delay = Duration::from_millis(self.cfg.autostart_delay_ms) + entry.delay;
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = ev_tx.send(Event::AutostartGo { entry, restored });
        });
    }

    // ── event handling ──────────────────────────────────────────────

    /// Project pending clients + active inhibitors into the Blockers
    /// property ("show which app is blocking", spec 02 §3).
    fn sync_blockers(&mut self) {
        let mut b: Vec<String> = self.machine.pending();
        for inh in self.inhibitors.snapshots() {
            b.push(format!("inhibitor: {} ({})", inh.who, inh.why));
        }
        self.shared_tx.set_blockers(b);
    }

    async fn handle_one(
        &mut self,
        ev: Event,
        state_path: &std::path::Path,
        _h: &std::path::Path,
    ) -> bool {
        match ev {
            Event::SigTerm => {
                let outputs = self.machine.force_end();
                self.apply_outputs(outputs, state_path).await;
                self.teardown_services().await;
                return true;
            }
            Event::SigHup => {
                tracing::info!(target: "session", "SIGHUP: config reload");
                if let Some(path) = std::env::var_os("LION_SESSION_CONFIG") {
                    match Config::load(PathBuf::from(&path).as_path()) {
                        Ok(new_cfg) => {
                            self.cfg.shutdown_timeout_ms = new_cfg.shutdown_timeout_ms;
                            self.cfg.autostart_delay_ms = new_cfg.autostart_delay_ms;
                            self.cfg.crash_loop = new_cfg.crash_loop;
                            tracing::info!(target: "session", "config reloaded (live keys)");
                        }
                        Err(e) => {
                            tracing::warn!(target: "session", "config reload failed ({e}); keeping current")
                        }
                    }
                }
            }
            Event::ChildExit { name, info } => {
                if self.ending {
                    return false;
                }
                if name == "compositor" {
                    tracing::warn!(
                        target: "session",
                        "compositor exited (code {:?}, abnormal={}) — ending session cleanly",
                        info.code,
                        info.abnormal
                    );
                    let outputs = self.machine.force_end();
                    self.apply_outputs(outputs, state_path).await;
                    self.teardown_services().await;
                    return true;
                }
                self.on_service_exit(&name, info);
            }
            Event::ChildReady { name } => {
                if let Some(s) = self.services.get_mut(&name) {
                    s.ready = true;
                    supervisor::on_healthy(&mut s.sup, Instant::now(), &self.cfg.crash_loop);
                } else if name == "compositor" {
                    if let Some(c) = self.compositor.as_mut() {
                        c.ready = true;
                    }
                }
                // autostart-app readiness: informational only
            }
            Event::CompositorReady => {
                if let Some(c) = self.compositor.as_mut() {
                    c.ready = true;
                }
            }
            Event::ServiceJobDone { unit, result } => {
                // Map a supervised unit back to its service.
                let name = self
                    .services
                    .iter()
                    .find(|(_, run)| run.spec.unit.as_deref() == Some(unit.as_str()))
                    .map(|(n, _)| n.clone());
                if self
                    .compositor
                    .as_ref()
                    .and_then(|c| c.spec.unit.as_deref())
                    .map(|u| u == unit)
                    .unwrap_or(false)
                {
                    if result == "failed" {
                        let outputs = self.machine.force_end();
                        self.apply_outputs(outputs, state_path).await;
                        self.teardown_services().await;
                        return true;
                    }
                    if let Some(c) = self.compositor.as_mut() {
                        c.ready = true;
                    }
                    return false;
                }
                match (name, result.as_str()) {
                    (Some(n), "failed") => {
                        self.on_service_exit(
                            &n,
                            ExitInfo {
                                code: Some(1),
                                abnormal: false,
                            },
                        );
                    }
                    (Some(n), "done") => {
                        if let Some(s) = self.services.get_mut(&n) {
                            s.ready = true;
                            supervisor::on_healthy(
                                &mut s.sup,
                                Instant::now(),
                                &self.cfg.crash_loop,
                            );
                        }
                    }
                    _ => {}
                }
            }
            Event::Respawn { name } => {
                if self.ending {
                    return false;
                }
                if let Some(spec) = self.services.get(&name).map(|s| s.spec.clone()) {
                    let _ = self.spawn_service(&spec).await;
                }
            }
            Event::AutostartGo { entry, restored } => {
                if self.ending {
                    return false;
                }
                let env = self.env.as_ref().expect("env built").child_env();
                let argv = entry.exec.clone();
                match self.deps.launcher.spawn(&entry.id, &argv, &env).await {
                    Ok(spawned) => {
                        self.attach_tasks(&format!("autostart:{}", entry.id), spawned);
                        self.autostarted
                            .insert(entry.id.clone(), entry.exec.clone());
                        tracing::info!(
                            target: "session",
                            "autostart launched {} (restored={restored})",
                            entry.id
                        );
                    }
                    Err(e) => {
                        tracing::warn!(target: "session", "autostart {} failed: {e}", entry.id);
                    }
                }
            }
            Event::QueryDeadline => {
                let outputs = self.machine.tick(Instant::now());
                self.apply_outputs(outputs, state_path).await;
                if self.machine.state() == "ended" {
                    self.teardown_services().await;
                    return true;
                }
            }
            Event::InhibitReq {
                what,
                who,
                why,
                owner,
                uid,
                reply,
            } => {
                let r = self.handle_inhibit(&what, &who, &why, &owner, uid).await;
                let _ = reply.send(r);
            }
            Event::InhibitorReleased { id } => {
                if self.inhibitors.remove(id).is_some() {
                    self.shared_tx
                        .set_inhibited(self.inhibitors.inhibited_actions());
                    tracing::info!(target: "inhibit", "inhibitor {id} released");
                }
            }
            Event::RegisterClient {
                app_id,
                owner,
                uid,
                pid,
                reply,
            } => {
                let r = self.handle_register(app_id, owner, uid, pid).await;
                let _ = reply.send(r);
            }
            Event::EndSessionReply { app_id, reply } => {
                let outputs = self.machine.ack(&app_id);
                let _ = reply.send(Ok(()));
                self.apply_outputs(outputs, state_path).await;
                if self.machine.state() == "ended" {
                    self.teardown_services().await;
                    return true;
                }
            }
            Event::ClientGone { owner } => {
                let apps: Vec<String> = self.owner_by_name.remove(&owner).unwrap_or_default();
                let outputs: Vec<Output> = apps
                    .iter()
                    .flat_map(|a| self.machine.client_gone(a))
                    .collect();
                for a in &apps {
                    self.clients.remove(a);
                }
                let released = self.inhibitors.retain_owner_gone(&owner);
                if !released.is_empty() {
                    self.shared_tx
                        .set_inhibited(self.inhibitors.inhibited_actions());
                    for r in &released {
                        tracing::info!(
                            target: "inhibit",
                            "inhibitor {} of vanished client auto-released",
                            r.id
                        );
                    }
                }
                self.apply_outputs(outputs, state_path).await;
                if self.machine.state() == "ended" {
                    self.teardown_services().await;
                    return true;
                }
            }
            Event::Lifecycle { action, uid, reply } => {
                let r = self.handle_lifecycle(action, uid, state_path).await;
                let _ = reply.send(r);
                if self.machine.state() == "ended" {
                    self.teardown_services().await;
                    return true;
                }
            }
            Event::Lock { uid, reply } => {
                let ok = self
                    .deps
                    .authz
                    .authorize(actions::LOCK, uid)
                    .await
                    .unwrap_or(false);
                let r = if ok {
                    let session_id = std::env::var("XDG_SESSION_ID").ok();
                    self.deps.logind.lock(session_id.as_deref()).await
                } else {
                    Err(Error::Denied("lock not permitted".into()))
                };
                let _ = reply.send(r);
            }
            Event::SwitchUser { uid, reply } => {
                let ok = self
                    .deps
                    .authz
                    .authorize(actions::SWITCH_USER, uid)
                    .await
                    .unwrap_or(false);
                let r = if ok {
                    let lock_all = self.deps.logind.lock(None).await;
                    let activate = match &self.cfg.greeter_session_id {
                        Some(id) => self.deps.logind.activate_session(id).await,
                        None => Ok(()),
                    };
                    lock_all.and(activate)
                } else {
                    Err(Error::Denied("switch-user not permitted".into()))
                };
                let _ = reply.send(r);
            }
        }
        false
    }

    async fn handle_inhibit(
        &mut self,
        what: &str,
        who: &str,
        why: &str,
        owner: &str,
        uid: u32,
    ) -> std::result::Result<std::os::unix::net::UnixStream, String> {
        if !self
            .deps
            .authz
            .authorize(actions::INHIBIT, uid)
            .await
            .unwrap_or(false)
        {
            return Err("inhibit not permitted".into());
        }
        if !self.inhibit_limit.allow(owner, Instant::now()) {
            return Err("inhibit rate limit exceeded".into());
        }
        let max = self.cfg.inhibit.max_active as usize;
        let id = self
            .inhibitors
            .add(what, who, why, owner, Instant::now(), max)?;
        let (ours, theirs) = inhibitors::inhibitor_fd_pair().map_err(|e| e.to_string())?;
        let ev_tx = self.ev_tx.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut sock = ours;
            let mut buf = [0u8; 8];
            loop {
                match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => continue,
                }
            }
            let _ = ev_tx.send(Event::InhibitorReleased { id });
        });
        self.shared_tx
            .set_inhibited(self.inhibitors.inhibited_actions());
        tracing::info!(target: "inhibit", "inhibitor {id}: {what} by {who} ({why})");
        Ok(theirs)
    }

    async fn handle_register(
        &mut self,
        app_id: String,
        owner: String,
        uid: u32,
        pid: u32,
    ) -> Result<()> {
        if app_id.is_empty() || app_id.len() > 256 {
            return Err(Error::InvalidParams("app_id must be 1..=256 bytes".into()));
        }
        if !self
            .deps
            .authz
            .authorize(actions::REGISTER, uid)
            .await
            .unwrap_or(false)
        {
            return Err(Error::Denied("register not permitted".into()));
        }
        if !self.register_limit.allow(&owner, Instant::now()) {
            return Err(Error::Denied("register rate limit exceeded".into()));
        }
        self.machine.register(&app_id).map_err(Error::Denied)?;
        self.clients.insert(
            app_id.clone(),
            ClientInfo {
                app_id: app_id.clone(),
                uid,
                pid,
            },
        );
        self.owner_by_name
            .entry(owner)
            .or_default()
            .push(app_id.clone());
        tracing::info!(target: "session", "client registered: {app_id} (pid {pid})");
        Ok(())
    }

    async fn handle_lifecycle(
        &mut self,
        action: Action,
        uid: u32,
        state_path: &std::path::Path,
    ) -> Result<()> {
        let action_id = match action {
            Action::Logout => actions::LOGOUT,
            Action::Restart => actions::RESTART,
            Action::Shutdown => actions::SHUTDOWN,
            Action::Suspend => actions::SUSPEND,
            Action::Hibernate => actions::HIBERNATE,
        };
        if !self
            .deps
            .authz
            .authorize(action_id, uid)
            .await
            .unwrap_or(false)
        {
            return Err(Error::Denied(format!("{} not permitted", action.name())));
        }
        let snapshots: Vec<InhibitorSnapshot> = self.inhibitors.snapshots();
        let outputs = self
            .machine
            .try_begin(action, Instant::now(), &snapshots)
            .map_err(Error::Denied)?;
        if self.machine.state() == "query-end-session" {
            self.shared_tx.set_state("query-end-session");
            let ev_tx = self.ev_tx.clone();
            let delay = Duration::from_millis(self.cfg.shutdown_timeout_ms);
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                let _ = ev_tx.send(Event::QueryDeadline);
            });
        }
        self.apply_outputs(outputs, state_path).await;
        Ok(())
    }

    async fn apply_outputs(&mut self, outputs: Vec<Output>, state_path: &std::path::Path) {
        for out in outputs {
            match out {
                Output::SignalQueryEndSession(flags) => {
                    let _ = self.sig_tx.send(SignalReq::QueryEndSession(flags));
                }
                Output::SignalEndSession(flags) => {
                    self.shared_tx.set_state("ending");
                    let _ = self.sig_tx.send(SignalReq::EndSession(flags));
                }
                Output::PersistState => {
                    if self.cfg.restore_apps {
                        let state = self.build_session_state();
                        if let Err(e) = state.save(state_path) {
                            tracing::warn!(target: "savestate", "persist failed: {e}");
                        } else {
                            tracing::info!(
                                target: "savestate",
                                "saved {} app(s) for restore",
                                state.apps.len()
                            );
                        }
                    }
                }
                Output::ForceKill => {
                    // teardown_services() (called right after) performs it
                }
                Output::ExecuteAction(action) => {
                    let r = match action {
                        Action::Logout => Ok(()),
                        Action::Restart => self.deps.logind.reboot().await,
                        Action::Shutdown => self.deps.logind.power_off().await,
                        Action::Suspend => self.deps.logind.suspend().await,
                        Action::Hibernate => self.deps.logind.hibernate().await,
                    };
                    if let Err(e) = r {
                        tracing::error!(target: "session", "execute {}: {e}", action.name());
                    }
                }
            }
        }
    }

    /// Remember autostarted/restored apps (their execs are known; spec:
    /// remember open apps and workspaces).
    fn build_session_state(&self) -> SessionState {
        let mut state = SessionState::default();
        for (id, exec) in &self.autostarted {
            state.upsert_app(id, exec.clone(), 0);
        }
        state
    }

    fn on_service_exit(&mut self, name: &str, info: ExitInfo) {
        let Some(run) = self.services.get_mut(name) else {
            return;
        };
        run.child = None;
        let now = Instant::now();
        let decision = supervisor::on_exit(
            &mut run.sup,
            run.spec.restart,
            &self.cfg.crash_loop,
            now,
            info.is_ok(),
        );
        self.apply_decision(name, decision);
    }

    fn apply_decision(&mut self, name: &str, decision: Decision) {
        match decision {
            Decision::RestartAfter(delay) => {
                let ev_tx = self.ev_tx.clone();
                let n = name.to_string();
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    let _ = ev_tx.send(Event::Respawn { name: n });
                });
                tracing::info!(target: "session", "{name} restart scheduled in {delay:?}");
            }
            Decision::GiveUpAndNotify => {
                let crashes = self
                    .services
                    .get(name)
                    .map(|r| r.sup.failures_in_window())
                    .unwrap_or(0);
                let reason = supervisor::failure_reason(crashes, self.cfg.crash_loop.window_ms);
                let notify = self
                    .services
                    .get_mut(name)
                    .map(|r| {
                        supervisor::should_notify(&mut r.sup, &self.cfg.crash_loop, Instant::now())
                    })
                    .unwrap_or(true);
                if notify {
                    let _ = self
                        .sig_tx
                        .send(SignalReq::ServiceFailed(name.to_string(), reason.clone()));
                    let notifier = self.deps.notifier.clone();
                    let summary = format!("LionOS: {name} keeps crashing");
                    let body = reason.clone();
                    tokio::spawn(async move {
                        if let Err(e) = notifier.notify(&summary, &body).await {
                            tracing::warn!(target: "session", "notification failed: {e}");
                        }
                    });
                }
                tracing::warn!(target: "session", "{name} gave up: {reason}");
            }
            Decision::StayDown => {
                tracing::info!(target: "session", "{name} stays down");
            }
        }
    }

    /// Stop everything: services in reverse plan order (dependents
    /// first), compositor last (spec 02 §3).
    async fn teardown_services(&mut self) {
        if self.ending {
            return;
        }
        self.ending = true;
        self.shared_tx.set_state("ending");
        let stop_order: Vec<String> = self
            .plan
            .as_ref()
            .map(|p| p.stop_order())
            .unwrap_or_default();
        let sd = self.deps.systemd.clone();
        let systemd_mode = self.deps.systemd_mode;
        for name in &stop_order {
            if let Some(run) = self.services.get_mut(name) {
                if let Some(child) = run.child.take() {
                    child.kill();
                } else if let (true, Some(sd), Some(unit)) = (systemd_mode, &sd, &run.spec.unit) {
                    let _ = sd.stop_unit(unit, "fail").await;
                }
            }
        }
        if let Some(comp) = self.compositor.as_mut() {
            if let Some(child) = comp.child.take() {
                child.kill();
            }
        }
        // Bounded grace so wait tasks observe the kills before we exit.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mocks::{
        MockAuthorizer, MockLauncher, MockLogind, MockNotifier, MockReadyWatch, MockSystemdUser,
        Script,
    };

    fn test_cfg() -> Config {
        Config {
            autostart_delay_ms: 0,
            startup: crate::config::StartupConfig {
                compositor_ready_timeout_ms: 500,
                shell_ready_timeout_ms: 800,
            },
            services: vec![
                crate::config::ServiceSpec {
                    name: "panel".into(),
                    unit: None,
                    exec: Some(vec!["panel".into()]),
                    after: vec![],
                    restart: RestartPolicy::Always,
                    ready_gate: true,
                },
                crate::config::ServiceSpec {
                    name: "wallpaper".into(),
                    unit: None,
                    exec: Some(vec!["wallpaper".into()]),
                    after: vec![],
                    restart: RestartPolicy::OnFailure,
                    ready_gate: true,
                },
            ],
            ..Config::default()
        }
    }

    fn deps() -> Deps {
        Deps {
            launcher: Arc::new(MockLauncher::default()),
            systemd: None,
            systemd_mode: false,
            logind: Arc::new(MockLogind::default()),
            authz: Arc::new(MockAuthorizer::default()),
            notifier: Arc::new(MockNotifier::default()),
            ready_watch: Arc::new(MockReadyWatch::default()),
            notify: Notify { sock: None },
        }
    }

    /// Drive the core: wait for a spawn, mark it ready, repeatedly.
    async fn make_ready(launcher: &MockLauncher, name: &str) {
        for _ in 0..100 {
            if launcher.make_ready(name).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("service {name} never appeared for readiness");
    }

    async fn next_signal(rx: &mut mpsc::UnboundedReceiver<SignalReq>) -> SignalReq {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("signal within 5s")
            .expect("channel open")
    }

    #[tokio::test]
    async fn compositor_never_ready_is_bad_start() {
        let (core, _s, _e) = SessionCore::new(test_cfg(), deps(), mpsc::unbounded_channel().0);
        let dir = tempfile::tempdir().unwrap();
        let hist = dir.path().join("h.json");
        let code = core.run(dir.path().join("s.json"), hist.clone()).await;
        assert_eq!(code, 1);
        assert_eq!(History::load(&hist).consecutive_bad_starts, 1);
    }

    #[tokio::test]
    async fn compositor_pre_ready_crash_is_bad_start() {
        let deps = Deps {
            launcher: Arc::new(MockLauncher::with_scripts(vec![(
                "compositor".into(),
                Script::ExitAfter {
                    delay: Duration::ZERO,
                    code: 1,
                    abnormal: false,
                },
            )])),
            ..deps()
        };
        let (core, _s, _e) = SessionCore::new(test_cfg(), deps, mpsc::unbounded_channel().0);
        let dir = tempfile::tempdir().unwrap();
        let hist = dir.path().join("h.json");
        let code = core.run(dir.path().join("s.json"), hist.clone()).await;
        assert_eq!(code, 1);
        assert_eq!(History::load(&hist).consecutive_bad_starts, 1);
    }

    #[tokio::test]
    async fn happy_path_ready_shutdown_and_env_import() {
        let cfg = test_cfg();
        let systemd = MockSystemdUser::default();
        let launcher = MockLauncher::default();
        let deps = Deps {
            launcher: Arc::new(launcher.clone()),
            systemd: Some(Arc::new(systemd.clone())),
            ..deps()
        };
        let (core, shared, ev_tx) = SessionCore::new(cfg, deps, mpsc::unbounded_channel().0);
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("s.json");
        let hist = dir.path().join("h.json");

        let runner = tokio::spawn(async move { core.run(state, hist).await });

        // Compositor + gates become ready.
        make_ready(&launcher, "compositor").await;
        make_ready(&launcher, "panel").await;
        make_ready(&launcher, "wallpaper").await;

        // startup completes; the first property flips to running.
        let mut rx = shared.state.clone();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if *rx.borrow_and_update() == "running" {
                break;
            }
            assert!(Instant::now() < deadline, "state never became running");
            rx.changed().await.unwrap();
        }
        // environment was imported into systemd --user
        assert!(!systemd.environment_imports().is_empty());
        assert!(systemd.environment_imports()[0]
            .iter()
            .any(|kv| kv.starts_with("XDG_CURRENT_DESKTOP=LionOS")));

        // graceful stop via SIGTERM event
        ev_tx.send(Event::SigTerm).unwrap();
        let code = tokio::time::timeout(Duration::from_secs(5), runner)
            .await
            .expect("core exits")
            .unwrap();
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn logout_flow_query_ack_end() {
        let cfg = test_cfg();
        let launcher = MockLauncher::default();
        let deps = Deps {
            launcher: Arc::new(launcher.clone()),
            ..deps()
        };
        let (sig_tx, mut sig_rx) = mpsc::unbounded_channel();
        let (core, mut shared, ev_tx) = SessionCore::new(cfg, deps, sig_tx);
        let dir = tempfile::tempdir().unwrap();

        let runner = tokio::spawn(async move {
            core.run(dir.path().join("s.json"), dir.path().join("h.json"))
                .await
        });
        make_ready(&launcher, "compositor").await;

        // SessionReady fires once startup completes; consume it.
        match next_signal(&mut sig_rx).await {
            SignalReq::SessionReady => {}
            other => panic!("expected SessionReady first, got {other:?}"),
        }

        // register a client first
        let (tx, rx) = oneshot::channel();
        ev_tx
            .send(Event::RegisterClient {
                app_id: "lion-text".into(),
                owner: ":1.42".into(),
                uid: 1000,
                pid: 4321,
                reply: tx,
            })
            .unwrap();
        rx.await.unwrap().unwrap();

        // Logout: query phase (client hasn't acked)
        let (tx, rx) = oneshot::channel();
        ev_tx
            .send(Event::Lifecycle {
                action: Action::Logout,
                uid: 1000,
                reply: tx,
            })
            .unwrap();
        rx.await.unwrap().unwrap();
        assert_eq!(*shared.state.borrow_and_update(), "query-end-session");
        match next_signal(&mut sig_rx).await {
            SignalReq::QueryEndSession(0) => {}
            other => panic!("expected QueryEndSession, got {other:?}"),
        }

        // client acks → EndSession → core exits
        let (tx, rx) = oneshot::channel();
        ev_tx
            .send(Event::EndSessionReply {
                app_id: "lion-text".into(),
                reply: tx,
            })
            .unwrap();
        rx.await.unwrap().unwrap();
        match next_signal(&mut sig_rx).await {
            SignalReq::EndSession(flags) => assert_eq!(flags, 0),
            other => panic!("expected EndSession, got {other:?}"),
        }
        let code = tokio::time::timeout(Duration::from_secs(5), runner)
            .await
            .expect("core exits after end")
            .unwrap();
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn inhibitor_blocks_logout_until_fd_released() {
        let cfg = test_cfg();
        let launcher = MockLauncher::default();
        let deps = Deps {
            launcher: Arc::new(launcher.clone()),
            ..deps()
        };
        let (core, mut shared, ev_tx) = SessionCore::new(cfg, deps, mpsc::unbounded_channel().0);
        let dir = tempfile::tempdir().unwrap();
        let runner = tokio::spawn(async move {
            core.run(dir.path().join("s.json"), dir.path().join("h.json"))
                .await
        });
        make_ready(&launcher, "compositor").await;

        // Take a logout inhibitor; keep the fd open.
        let (tx, rx) = oneshot::channel();
        ev_tx
            .send(Event::InhibitReq {
                what: "logout".into(),
                who: "lion-text".into(),
                why: "unsaved document".into(),
                owner: ":1.7".into(),
                uid: 1000,
                reply: tx,
            })
            .unwrap();
        let fd = rx.await.unwrap().unwrap();
        assert_eq!(
            *shared.inhibited_actions.borrow_and_update(),
            vec!["logout".to_string()]
        );

        // Logout is refused while the inhibitor is held.
        let (tx, rx) = oneshot::channel();
        ev_tx
            .send(Event::Lifecycle {
                action: Action::Logout,
                uid: 1000,
                reply: tx,
            })
            .unwrap();
        let err = rx.await.unwrap().unwrap_err();
        assert!(err.to_string().contains("inhibited"), "got: {err}");

        // Client drops the fd → auto-release → logout proceeds.
        drop(fd);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !shared.inhibited_actions.borrow_and_update().is_empty() {
            assert!(Instant::now() < deadline, "inhibitor never released");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let (tx, rx) = oneshot::channel();
        ev_tx
            .send(Event::Lifecycle {
                action: Action::Logout,
                uid: 1000,
                reply: tx,
            })
            .unwrap();
        rx.await.unwrap().unwrap();
        ev_tx.send(Event::SigTerm).unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), runner).await;
    }

    #[tokio::test]
    async fn suspend_executes_directly_via_logind() {
        let cfg = test_cfg();
        let logind = MockLogind::default();
        let launcher = MockLauncher::default();
        let deps = Deps {
            launcher: Arc::new(launcher.clone()),
            logind: Arc::new(logind.clone()),
            ..deps()
        };
        let (core, mut shared, ev_tx) = SessionCore::new(cfg, deps, mpsc::unbounded_channel().0);
        let dir = tempfile::tempdir().unwrap();
        let runner = tokio::spawn(async move {
            core.run(dir.path().join("s.json"), dir.path().join("h.json"))
                .await
        });
        make_ready(&launcher, "compositor").await;

        // Suspend does not end the session: state stays running and the
        // logind mock records the call.
        let (tx, rx) = oneshot::channel();
        ev_tx
            .send(Event::Lifecycle {
                action: Action::Suspend,
                uid: 1000,
                reply: tx,
            })
            .unwrap();
        rx.await.unwrap().unwrap();
        assert_eq!(logind.calls(), vec!["suspend".to_string()]);
        assert_eq!(*shared.state.borrow_and_update(), "running");
        ev_tx.send(Event::SigTerm).unwrap();
        let code = tokio::time::timeout(Duration::from_secs(5), runner)
            .await
            .expect("core exits")
            .unwrap();
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn crash_loop_gives_up_and_notifies_once() {
        let mut cfg = test_cfg();
        cfg.crash_loop = crate::config::CrashLoopConfig {
            max_failures: 3,
            window_ms: 60_000,
            backoff_start_ms: 10,
            backoff_max_ms: 40,
            notify_coalesce_ms: 60_000,
        };
        // wallpaper crashes instantly, repeatedly (policy OnFailure).
        let launcher = MockLauncher::with_scripts(vec![
            ("compositor".into(), Script::Run),
            (
                "wallpaper".into(),
                Script::ExitAfter {
                    delay: Duration::ZERO,
                    code: 1,
                    abnormal: false,
                },
            ),
        ]);
        let notifier = MockNotifier::default();
        let deps = Deps {
            launcher: Arc::new(launcher.clone()),
            notifier: Arc::new(notifier.clone()),
            ..deps()
        };
        let (sig_tx, mut sig_rx) = mpsc::unbounded_channel();
        let (core, _shared, ev_tx) = SessionCore::new(cfg, deps, sig_tx);
        let dir = tempfile::tempdir().unwrap();
        let runner = tokio::spawn(async move {
            core.run(dir.path().join("s.json"), dir.path().join("h.json"))
                .await
        });

        make_ready(&launcher, "compositor").await;
        make_ready(&launcher, "panel").await;

        // wallpaper crashes → 3 strikes → ServiceFailed (exactly one
        // notification: coalescing).
        let mut failures = 0;
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            assert!(Instant::now() < deadline, "no ServiceFailed observed");
            match tokio::time::timeout(Duration::from_secs(2), sig_rx.recv()).await {
                Ok(Some(SignalReq::ServiceFailed(name, reason))) => {
                    assert_eq!(name, "wallpaper");
                    assert!(reason.contains("crashed 3 times"), "got: {reason}");
                    failures += 1;
                    break;
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => continue,
            }
        }
        assert_eq!(failures, 1);
        // …and the user-visible notification fired.
        let deadline = Instant::now() + Duration::from_secs(2);
        while notifier.sent.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline, "no notification sent");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(notifier.sent.lock().unwrap()[0].0.contains("wallpaper"));

        ev_tx.send(Event::SigTerm).unwrap();
        let code = tokio::time::timeout(Duration::from_secs(5), runner)
            .await
            .expect("exits")
            .unwrap();
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn unauthorized_caller_denied() {
        let launcher = MockLauncher::default();
        let deps = Deps {
            launcher: Arc::new(launcher.clone()),
            authz: Arc::new(MockAuthorizer {
                allow_all: false,
                ..Default::default()
            }),
            ..deps()
        };
        let (core, _shared, ev_tx) =
            SessionCore::new(test_cfg(), deps, mpsc::unbounded_channel().0);
        let dir = tempfile::tempdir().unwrap();
        let runner = tokio::spawn(async move {
            core.run(dir.path().join("s.json"), dir.path().join("h.json"))
                .await
        });
        make_ready(&launcher, "compositor").await;
        let (tx, rx) = oneshot::channel();
        ev_tx
            .send(Event::Lifecycle {
                action: Action::Logout,
                uid: 1234,
                reply: tx,
            })
            .unwrap();
        let r = tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .unwrap()
            .unwrap();
        assert!(r.is_err());
        ev_tx.send(Event::SigTerm).unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), runner).await;
    }
}
