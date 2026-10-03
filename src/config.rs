#![forbid(unsafe_code)]
//! Configuration loading and validation (the `lion-config` client).
//!
//! Spec 02 §5 keys, plus documented namespaced extensions for supervision,
//! autostart, crash-loop and authorization policy. Canonical schema ships at
//! `packaging/lion-config/session.schema.json` (embedded here so
//! `--print-schema` never depends on the install tree) and the loader
//! rejects unknown fields — fail closed on anything unexpected.
//!
//! Spec keys (defaults):
//! - `session.shutdown_timeout_ms` (8000)
//! - `session.restore_apps` (false)
//! - `session.autostart_delay_ms` (1500)

use crate::error::{Error, Result};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Canonical schema, embedded for `--print-schema`.
pub const SCHEMA_JSON: &str = include_str!("../packaging/lion-config/session.schema.json");

pub const DEFAULT_CONFIG_PATH: &str = "/etc/lion/session.json";

/// Restart policy per supervised service (spec 02 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    /// Restart on any exit (shell services: the user keeps working).
    #[default]
    Always,
    /// Restart only on non-zero exit.
    OnFailure,
    /// Never restart; the exit stands.
    Never,
}

/// One supervised service in the session plan.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceSpec {
    /// Short name used in ordering, logs and `ServiceFailed` signals.
    pub name: String,
    /// systemd user unit to start (systemd mode; preferred when present).
    #[serde(default)]
    pub unit: Option<String>,
    /// argv to spawn directly (direct mode; required when `unit` is absent).
    #[serde(default)]
    pub exec: Option<Vec<String>>,
    /// Names that must be started first (dependency edges).
    #[serde(default)]
    pub after: Vec<String>,
    /// `always` / `on-failure` / `never`.
    #[serde(default)]
    pub restart: RestartPolicy,
    /// Whether this service gates `SessionReady` (panel + wallpaper).
    #[serde(default)]
    pub ready_gate: bool,
}

impl ServiceSpec {
    /// How to run this service in the given mode: unit name or argv.
    pub fn runnable(&self) -> Option<Runnable> {
        match (&self.unit, &self.exec) {
            (Some(u), _) => Some(Runnable::Unit(u.clone())),
            (None, Some(argv)) if !argv.is_empty() => Some(Runnable::Exec(argv.clone())),
            _ => None,
        }
    }
}

/// What a service resolves to in the active mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Runnable {
    Unit(String),
    Exec(Vec<String>),
}

/// The compositor is special: if it dies the session ends (spec 02 §3).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompositorSpec {
    /// argv for direct mode (default: the Smithay compositor binary).
    #[serde(default = "default_compositor_exec")]
    pub exec: Vec<String>,
    /// systemd user unit in systemd mode.
    #[serde(default = "default_compositor_unit")]
    pub unit: Option<String>,
    /// Wayland socket basename to await before starting shell services.
    #[serde(default = "default_wayland_display")]
    pub wayland_display: String,
}

impl Default for CompositorSpec {
    fn default() -> Self {
        serde_json::from_str("{}").expect("static defaults parse")
    }
}

fn default_compositor_exec() -> Vec<String> {
    vec!["/usr/lib/lion/lion-compositor".into()]
}
fn default_compositor_unit() -> Option<String> {
    Some("lion-compositor.service".into())
}
fn default_wayland_display() -> String {
    "wayland-0".into()
}

/// Crash-loop detection window (spec 02 §3: "after N crashes in M seconds,
/// stop retrying and notify the user").
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrashLoopConfig {
    /// N failures within the window → give up.
    #[serde(default = "default_cl_failures")]
    pub max_failures: u32,
    /// M, the sliding window length.
    #[serde(default = "default_cl_window")]
    pub window_ms: u64,
    /// First retry delay; doubles from here.
    #[serde(default = "default_cl_backoff_start")]
    pub backoff_start_ms: u64,
    /// Retry delay cap.
    #[serde(default = "default_cl_backoff_max")]
    pub backoff_max_ms: u64,
    /// Minimum gap between `ServiceFailed` notifications for the same
    /// service — one notification, not a storm (spec 02 §6).
    #[serde(default = "default_cl_notify")]
    pub notify_coalesce_ms: u64,
}

impl Default for CrashLoopConfig {
    fn default() -> Self {
        serde_json::from_str("{}").expect("static defaults parse")
    }
}

fn default_cl_failures() -> u32 {
    5
}
fn default_cl_window() -> u64 {
    15_000
}
fn default_cl_backoff_start() -> u64 {
    100
}
fn default_cl_backoff_max() -> u64 {
    5000
}
fn default_cl_notify() -> u64 {
    5000
}

/// Inhibitor abuse limits (spec 02 §8: rate-limit expensive calls).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InhibitConfig {
    /// Max `Inhibit()` calls per caller per minute; excess is denied.
    #[serde(default = "default_inh_rate")]
    pub rate_per_minute: u32,
    /// Hard cap on simultaneously held inhibitors.
    #[serde(default = "default_inh_max")]
    pub max_active: u32,
}

impl Default for InhibitConfig {
    fn default() -> Self {
        serde_json::from_str("{}").expect("static defaults parse")
    }
}

fn default_inh_rate() -> u32 {
    30
}
fn default_inh_max() -> u32 {
    64
}

/// Safe-mode trigger (spec 02 §3).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SafeModeConfig {
    /// Consecutive bad starts (compositor crash before ready) that trip
    /// safe mode on the next boot.
    #[serde(default = "default_sm_threshold")]
    pub threshold: u32,
    /// Services to keep in safe mode (compositor is always kept).
    #[serde(default = "default_sm_minimal")]
    pub minimal_services: Vec<String>,
}

impl Default for SafeModeConfig {
    fn default() -> Self {
        serde_json::from_str("{}").expect("static defaults parse")
    }
}

fn default_sm_threshold() -> u32 {
    2
}
fn default_sm_minimal() -> Vec<String> {
    vec!["terminal".into(), "settings".into()]
}

/// Environment construction inputs (spec 02 §3 startup orchestration).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentConfig {
    /// Extra KEY=VALUE pairs applied last (overrides).
    #[serde(default)]
    pub extra: std::collections::BTreeMap<String, String>,
    /// Environment variables to import from the launching environment
    /// (greeter/systemd user session) into the session environment.
    #[serde(default = "default_env_pass")]
    pub import: Vec<String>,
}

impl Default for EnvironmentConfig {
    fn default() -> Self {
        serde_json::from_str("{}").expect("static defaults parse")
    }
}

fn default_env_pass() -> Vec<String> {
    vec![
        "LANG".into(),
        "LANGUAGE".into(),
        "LC_CTYPE".into(),
        "LC_NUMERIC".into(),
        "LC_TIME".into(),
        "LC_COLLATE".into(),
        "LC_MONETARY".into(),
        "LC_MESSAGES".into(),
        "LC_PAPER".into(),
        "LC_NAME".into(),
        "LC_ADDRESS".into(),
        "LC_TELEPHONE".into(),
        "LC_MEASUREMENT".into(),
        "LC_IDENTIFICATION".into(),
        "TZ".into(),
    ]
}

/// Authorization backend wiring (spec 02 §8: authorize through lion-auth).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// lion-auth bus name; fail closed when unreachable (spec §8).
    #[serde(default = "default_auth_bus")]
    pub bus_name: String,
    /// Per-call timeout.
    #[serde(default = "default_auth_timeout")]
    pub timeout_ms: u64,
    /// uids implicitly allowed session-scoped actions (the session owner is
    /// always allowed). Power actions additionally need power_allowed_uids.
    #[serde(default)]
    pub allowed_uids: Vec<u32>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        serde_json::from_str("{}").expect("static defaults parse")
    }
}

fn default_auth_bus() -> String {
    "os.lionos.Auth1".into()
}
fn default_auth_timeout() -> u64 {
    500
}

/// Startup orchestration timeouts.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupConfig {
    /// Max wait for the compositor Wayland socket.
    #[serde(default = "default_su_compositor")]
    pub compositor_ready_timeout_ms: u64,
    /// Max wait for ready-gate services after they started.
    #[serde(default = "default_su_shell")]
    pub shell_ready_timeout_ms: u64,
}

impl Default for StartupConfig {
    fn default() -> Self {
        serde_json::from_str("{}").expect("static defaults parse")
    }
}

fn default_su_compositor() -> u64 {
    15_000
}
fn default_su_shell() -> u64 {
    20_000
}

/// D-Bus service tuning.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BusConfig {
    /// Well-known name to own.
    #[serde(default = "default_bus_name")]
    pub name: String,
    /// Max `RegisterClient()` calls per caller per minute.
    #[serde(default = "default_bus_reg_rate")]
    pub register_rate_per_minute: u32,
}

impl Default for BusConfig {
    fn default() -> Self {
        serde_json::from_str("{}").expect("static defaults parse")
    }
}

fn default_bus_name() -> String {
    "os.lionos.Session1".into()
}
fn default_bus_reg_rate() -> u32 {
    30
}

/// Full `session.*` tree.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Spec: wait before force-killing apps (ms).
    #[serde(default = "default_shutdown_timeout")]
    pub shutdown_timeout_ms: u64,
    /// Spec: opt-in session restore.
    #[serde(default)]
    pub restore_apps: bool,
    /// Spec: base delay for autostart apps (ms).
    #[serde(default = "default_autostart_delay")]
    pub autostart_delay_ms: u64,
    /// Desktop id used for OnlyShowIn/NotShowIn matching.
    #[serde(default = "default_desktop_name")]
    pub desktop_name: String,
    /// Where history/state live; default resolves from XDG_STATE_HOME.
    #[serde(default)]
    pub state_dir: Option<PathBuf>,
    /// XDG autostart dirs, in XDG order (later entries override earlier).
    #[serde(default = "default_autostart_dirs")]
    pub autostart_dirs: Vec<PathBuf>,
    /// Supervised shell services.
    #[serde(default = "default_services")]
    pub services: Vec<ServiceSpec>,
    /// The compositor.
    #[serde(default)]
    pub compositor: CompositorSpec,
    /// Crash-loop / backoff tuning.
    #[serde(default)]
    pub crash_loop: CrashLoopConfig,
    /// Inhibitor limits.
    #[serde(default)]
    pub inhibit: InhibitConfig,
    /// uids allowed to trigger power actions on top of the lion-auth path
    /// (empty = lion-auth + root only).
    #[serde(default)]
    pub power_allowed_uids: Vec<u32>,
    /// Greeter session id (logind) for switch-user; None = rely on seat.
    #[serde(default)]
    pub greeter_session_id: Option<String>,
    /// Safe mode policy.
    #[serde(default)]
    pub safe_mode: SafeModeConfig,
    /// Environment construction.
    #[serde(default)]
    pub environment: EnvironmentConfig,
    /// lion-auth wiring.
    #[serde(default)]
    pub lion_auth: AuthConfig,
    /// Startup timeouts.
    #[serde(default)]
    pub startup: StartupConfig,
    /// Bus tuning.
    #[serde(default)]
    pub bus: BusConfig,
}

impl Default for Config {
    fn default() -> Self {
        serde_json::from_str("{}").expect("static defaults parse")
    }
}

fn default_shutdown_timeout() -> u64 {
    8000
}
fn default_autostart_delay() -> u64 {
    1500
}
fn default_desktop_name() -> String {
    "LionOS".into()
}
fn default_autostart_dirs() -> Vec<PathBuf> {
    vec!["/etc/xdg/autostart".into(), "~/.config/autostart".into()]
}
fn default_services() -> Vec<ServiceSpec> {
    vec![
        ServiceSpec {
            name: "panel".into(),
            unit: Some("lion-panel.service".into()),
            exec: Some(vec!["/usr/lib/lion/lion-panel".into()]),
            after: vec![],
            restart: RestartPolicy::Always,
            ready_gate: true,
        },
        ServiceSpec {
            name: "wallpaper".into(),
            unit: Some("lion-wallpaper.service".into()),
            exec: Some(vec!["/usr/lib/lion/lion-wallpaper".into()]),
            after: vec![],
            restart: RestartPolicy::Always,
            ready_gate: true,
        },
        ServiceSpec {
            name: "notifications".into(),
            unit: Some("lion-notifications.service".into()),
            exec: Some(vec!["/usr/lib/lion/lion-notifications".into()]),
            after: vec![],
            restart: RestartPolicy::OnFailure,
            ready_gate: false,
        },
        ServiceSpec {
            name: "terminal".into(),
            unit: Some("lion-terminal.service".into()),
            exec: Some(vec!["/usr/lib/lion/lion-terminal".into()]),
            after: vec!["panel".into()],
            restart: RestartPolicy::OnFailure,
            ready_gate: false,
        },
        ServiceSpec {
            name: "settings".into(),
            unit: Some("lion-settings.service".into()),
            exec: Some(vec!["/usr/lib/lion/lion-settings".into()]),
            after: vec!["panel".into()],
            restart: RestartPolicy::OnFailure,
            ready_gate: false,
        },
    ]
}

impl Config {
    /// Load from a JSON file and validate (expects the `{"session":{…}}`
    /// lion-config key-file shape).
    pub fn load(path: &Path) -> Result<Config> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("cannot read {}: {e}", path.display())))?;
        Config::parse(&raw).map_err(|e| match e {
            Error::Config(m) => Error::Config(format!("{}: {m}", path.display())),
            other => other,
        })
    }

    /// Load from an inline JSON string (tests, `--check-config` stdin).
    /// Accepts the file form (`{"session":{...}}`) — the `lion-config`
    /// key-file convention — and requires the wrapper (fail closed on
    /// malformed structure).
    pub fn parse(raw: &str) -> Result<Config> {
        let v: serde_json::Value =
            serde_json::from_str(raw).map_err(|e| Error::Config(format!("{e}")))?;
        let inner = v
            .get("session")
            .cloned()
            .ok_or_else(|| Error::Config("missing top-level \"session\" object".into()))?;
        let cfg: Config =
            serde_json::from_value(inner).map_err(|e| Error::Config(format!("{e}")))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Cross-field validation (fail closed; spec 02 §8).
    pub fn validate(&self) -> Result<()> {
        if !(500..=600_000).contains(&self.shutdown_timeout_ms) {
            return Err(Error::Config(format!(
                "session.shutdown_timeout_ms out of range [500, 600000]: {}",
                self.shutdown_timeout_ms
            )));
        }
        if self.autostart_delay_ms > 60_000 {
            return Err(Error::Config(format!(
                "session.autostart_delay_ms out of range [0, 60000]: {}",
                self.autostart_delay_ms
            )));
        }
        let cl = &self.crash_loop;
        if cl.max_failures < 2 || cl.max_failures > 100 {
            return Err(Error::Config(format!(
                "session.crash_loop.max_failures out of range [2, 100]: {}",
                cl.max_failures
            )));
        }
        if cl.window_ms < 1_000 {
            return Err(Error::Config(format!(
                "session.crash_loop.window_ms below 1000: {}",
                cl.window_ms
            )));
        }
        if cl.backoff_start_ms == 0 || cl.backoff_max_ms < cl.backoff_start_ms {
            return Err(Error::Config(
                "session.crash_loop backoff_max_ms must be >= backoff_start_ms (>= 1)".into(),
            ));
        }
        if self.inhibit.rate_per_minute == 0 || self.inhibit.max_active == 0 {
            return Err(Error::Config("session.inhibit limits must be >= 1".into()));
        }

        // Services: unique names, runnable, deps known.
        let mut names = HashSet::new();
        for s in &self.services {
            if s.name.is_empty() {
                return Err(Error::Config("service with empty name".into()));
            }
            if !names.insert(s.name.clone()) {
                return Err(Error::Config(format!("duplicate service name {}", s.name)));
            }
            if s.runnable().is_none() {
                return Err(Error::Config(format!(
                    "service {} has neither unit nor non-empty exec",
                    s.name
                )));
            }
        }
        for s in &self.services {
            for dep in &s.after {
                if !names.contains(dep) {
                    return Err(Error::Config(format!(
                        "service {} depends on unknown service {}",
                        s.name, dep
                    )));
                }
            }
        }
        if self.services.is_empty() && self.compositor.exec.is_empty() {
            return Err(Error::Config(
                "no services and no compositor command".into(),
            ));
        }
        if self.safe_mode.threshold == 0 {
            return Err(Error::Config(
                "session.safe_mode.threshold must be >= 1".into(),
            ));
        }
        if self.startup.compositor_ready_timeout_ms < 500
            || self.startup.shell_ready_timeout_ms < 500
        {
            return Err(Error::Config(
                "session.startup timeouts must be >= 500ms".into(),
            ));
        }
        if self.bus.name.is_empty() {
            return Err(Error::Config("session.bus.name must not be empty".into()));
        }
        // lion_auth.bus_name == "" disables lion-auth (local policy);
        // documented in DESIGN.md and session.schema.json.
        if self.lion_auth.bus_name.len() > 128 {
            return Err(Error::Config("session.lion_auth.bus_name too long".into()));
        }
        Ok(())
    }

    /// Effective state dir (XDG_STATE_HOME or HOME/.local/state).
    pub fn state_dir(&self) -> PathBuf {
        if let Some(p) = &self.state_dir {
            return p.clone();
        }
        let xdg = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")));
        match xdg {
            Some(base) => base.join("lion-session"),
            None => PathBuf::from("/tmp/lion-session"),
        }
    }

    /// Services kept in safe mode: compositor + the intersection of
    /// configured minimal services with defined services (warn on misses).
    pub fn safe_mode_services(&self) -> Vec<ServiceSpec> {
        let defined: HashSet<&str> = self.services.iter().map(|s| s.name.as_str()).collect();
        let mut keep: Vec<ServiceSpec> = self
            .services
            .iter()
            .filter(|s| self.safe_mode.minimal_services.iter().any(|m| m == &s.name))
            .cloned()
            .collect();
        for miss in &self.safe_mode.minimal_services {
            if !defined.contains(miss.as_str()) {
                tracing::warn!(target: "config", "safe_mode service {miss} is not defined; skipped");
            }
        }
        // Stable order matching the configured list.
        keep.sort_by_key(|s| {
            self.safe_mode
                .minimal_services
                .iter()
                .position(|m| m == &s.name)
                .unwrap_or(usize::MAX)
        });
        keep
    }

    /// Expand `~` in autostart dirs against $HOME (XDG conventions).
    pub fn autostart_dirs_expanded(&self) -> Vec<PathBuf> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        self.autostart_dirs
            .iter()
            .map(|d| {
                if d.starts_with("~") {
                    match &home {
                        Some(h) => h.join(d.strip_prefix("~/").unwrap_or(d.as_path())),
                        None => PathBuf::from("/nonexistent"),
                    }
                } else {
                    d.clone()
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        let cfg = Config::default();
        cfg.validate().expect("defaults validate");
        assert_eq!(cfg.shutdown_timeout_ms, 8000);
        assert!(!cfg.restore_apps);
        assert_eq!(cfg.autostart_delay_ms, 1500);
    }

    #[test]
    fn spec_keys_load() {
        let cfg = Config::parse(
            r#"{"session":{"shutdown_timeout_ms":4000,"restore_apps":true,"autostart_delay_ms":500}}"#,
        )
        .unwrap();
        assert_eq!(cfg.shutdown_timeout_ms, 4000);
        assert!(cfg.restore_apps);
        assert_eq!(cfg.autostart_delay_ms, 500);
    }

    #[test]
    fn unknown_fields_rejected() {
        let e = Config::parse(r#"{"session":{"shutdown_timeout":4000}}"#).unwrap_err();
        assert!(matches!(e, Error::Config(_)));
    }

    #[test]
    fn missing_required_runnable_rejected() {
        let e = Config::parse(r#"{"session":{"services":[{"name":"x"}]}}"#).unwrap_err();
        assert!(e.to_string().contains("neither unit nor"));
    }

    #[test]
    fn unknown_dependency_rejected() {
        let e = Config::parse(
            r#"{"session":{"services":[{"name":"x","exec":["/bin/true"],"after":["ghost"]}]}}"#,
        )
        .unwrap_err();
        assert!(e.to_string().contains("unknown service ghost"));
    }

    #[test]
    fn duplicate_names_rejected() {
        let e = Config::parse(
            r#"{"session":{"services":[
                {"name":"x","exec":["/bin/true"]},
                {"name":"x","exec":["/bin/false"]}
            ]}}"#,
        )
        .unwrap_err();
        assert!(e.to_string().contains("duplicate service name x"));
    }

    #[test]
    fn shutdown_timeout_bounds() {
        let e = Config::parse(r#"{"session":{"shutdown_timeout_ms":100}}"#).unwrap_err();
        assert!(e.to_string().contains("shutdown_timeout_ms"));
    }

    #[test]
    fn crash_loop_backoff_bounds() {
        let e = Config::parse(
            r#"{"session":{"crash_loop":{"backoff_max_ms":50,"backoff_start_ms":100}}}"#,
        )
        .unwrap_err();
        assert!(e.to_string().contains("backoff_max_ms"));
    }

    #[test]
    fn safe_mode_intersection_and_order() {
        let cfg = Config::parse(
            r#"{"session":{"services":[
                {"name":"panel","exec":["/bin/true"]},
                {"name":"settings","exec":["/bin/true"]},
                {"name":"terminal","exec":["/bin/true"]}
            ],
            "safe_mode":{"minimal_services":["terminal","settings","ghost"]}}}"#,
        )
        .unwrap();
        let sm = cfg.safe_mode_services();
        let names: Vec<&str> = sm.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["terminal", "settings"]);
    }

    #[test]
    fn state_dir_respects_xdg() {
        let cfg = Config::default();
        // HOME is set in the test environment; assert the join shape.
        let dir = cfg.state_dir();
        assert!(dir.ends_with("lion-session"));
    }

    #[test]
    fn autostart_tilde_expansion() {
        let cfg = Config::default();
        let dirs = cfg.autostart_dirs_expanded();
        assert_eq!(dirs.len(), 2);
        assert!(!dirs[1].starts_with("~"));
        assert!(dirs[1].ends_with("autostart"));
    }

    #[test]
    fn runnable_prefers_unit() {
        let s = ServiceSpec {
            name: "x".into(),
            unit: Some("x.service".into()),
            exec: Some(vec!["/bin/true".into()]),
            after: vec![],
            restart: RestartPolicy::Never,
            ready_gate: false,
        };
        assert_eq!(s.runnable(), Some(Runnable::Unit("x.service".into())));
        let s2 = ServiceSpec { unit: None, ..s };
        assert_eq!(
            s2.runnable(),
            Some(Runnable::Exec(vec!["/bin/true".into()]))
        );
    }
}
