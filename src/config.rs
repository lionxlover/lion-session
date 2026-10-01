//! Configuration: built-in defaults, overlaid by `/etc/lionos/session.toml`,
//! overlaid by `~/.config/lionos/session.toml` (or, if
//! `$LION_SESSION_CONFIG` is set, that one file replaces both).
//!
//! A broken config file is logged and ignored -- it must never be able to
//! stop someone from logging in. `--check-config` reports problems without
//! starting a session, for packagers and login-debugging.

use serde::Deserialize;
use std::path::PathBuf;

use crate::idle::IdlePolicy;

/// Startup ordering group. Lower starts first. TOML entries default per
/// component below; XDG autostart entries always land in phase 3.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, PartialOrd, Ord, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    /// 0: display-critical (wallpaper, colour daemons)
    Display = 0,
    /// 1: shell chrome (panel, dock, hotkeys)
    Shell = 1,
    /// 2: session services (idle manager, OSD)
    SessionServices = 2,
    /// 3: external XDG autostart apps and user extras
    #[default]
    Applications = 3,
}

/// Phase used for XDG desktop entries (`Applications`).
pub const XDG_PHASE: Phase = Phase::Applications;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Compositor {
    pub command: String,
    pub args: Vec<String>,
    /// Wayland socket name the compositor must create (exported as
    /// WAYLAND_DISPLAY), e.g. "wayland-1".
    pub wayland_display: String,
    pub ready_timeout_ms: u64,
    /// Restart the compositor when it crashes (GNOME auto-restarts the
    /// shell; we do the same for any compositor, with crash-loop backoff).
    pub restart: bool,
    /// Give up (and end the session) after this many compositor
    /// restarts inside `crash_window_ms`.
    pub max_restarts: u32,
    pub crash_window_ms: u64,
}

impl Default for Compositor {
    fn default() -> Self {
        Self {
            command: "lion-compositor".into(),
            args: vec![],
            wayland_display: "wayland-1".into(),
            ready_timeout_ms: 15_000,
            restart: true,
            max_restarts: 3,
            crash_window_ms: 60_000,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    Never,
    #[default]
    OnFailure,
    Always,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct App {
    pub name: String,
    /// Defaults to `name`.
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub delay_ms: u64,
    #[serde(default)]
    pub restart: RestartPolicy,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Startup ordering group; see [`Phase`].
    #[serde(default)]
    pub phase: Phase,
    /// 0.3.0 per-app resource limits (applied via systemd-run scopes;
    /// logged-and-ignored on the direct-launch fallback). Values are
    /// systemd cgroup property values: `"512M"`, `"2G"`, plain numbers.
    #[serde(default)]
    pub memory_max: Option<String>,
    /// CPU weight 1..=10000 (systemd `CPUWeight=`).
    #[serde(default)]
    pub cpu_weight: Option<u32>,
    /// Task (thread/process) cap for the app's cgroup.
    #[serde(default)]
    pub tasks_max: Option<u32>,
}

impl App {
    /// The systemd cgroup properties this app requests, as
    /// `--property=KEY=VALUE` arguments. Empty when unconfigured.
    /// Values are validated *here* — the single point where config
    /// data becomes argv: a NUL would abort posix_spawn, and anything
    /// but systemd's own value grammar would make the whole scope fail
    /// to start. Invalid values are skipped with a warning (fail-open:
    /// the app starts unlimited, the config error is visible in the
    /// journal and `--check-config`).
    pub fn cgroup_properties(&self) -> Vec<String> {
        let mut v = Vec::new();
        if let Some(m) = &self.memory_max {
            if valid_property_value(m) {
                v.push(format!("--property=MemoryMax={m}"));
            } else {
                tracing::warn!(value = %m, "invalid memory-max ignored");
            }
        }
        if let Some(w) = self.cpu_weight {
            if (1..=10_000).contains(&w) {
                v.push(format!("--property=CPUWeight={w}"));
            } else {
                tracing::warn!(value = w, "cpu-weight outside 1..=10000 ignored");
            }
        }
        if let Some(t) = self.tasks_max {
            if t > 0 {
                v.push(format!("--property=TasksMax={t}"));
            }
        }
        v
    }
}

/// systemd size/percentage/relative value grammar: alphanumerics plus
/// `. % + -` (covers `512M`, `2.5G`, `15%`, `+100M`, `infinity`).
/// NUL is impossible here by construction; everything else unusual
/// (shell metacharacters, whitespace) is systemd's own parser's
/// problem — the value travels as one argv element, never a shell.
fn valid_property_value(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 32
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'%' | b'+' | b'-'))
}

fn yes() -> bool {
    true
}

impl App {
    fn named(name: &str, phase: Phase) -> Self {
        Self {
            name: name.into(),
            command: None,
            args: vec![],
            delay_ms: 0,
            restart: RestartPolicy::OnFailure,
            enabled: true,
            phase,
            memory_max: None,
            cpu_weight: None,
            tasks_max: None,
        }
    }
    pub fn command(&self) -> &str {
        self.command.as_deref().unwrap_or(&self.name)
    }
}

/// Where Lock and power actions are forwarded, on the session bus.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Services {
    pub locker_service: String,
    pub locker_path: String,
    pub locker_interface: String,
    pub power_service: String,
    pub power_path: String,
    pub power_interface: String,
}

impl Default for Services {
    fn default() -> Self {
        Self {
            locker_service: "os.lionos.Locker".into(),
            locker_path: "/os/lionos/Locker".into(),
            locker_interface: "os.lionos.Locker1".into(),
            power_service: "os.lionos.Power".into(),
            power_path: "/os/lionos/Power".into(),
            power_interface: "os.lionos.Power1".into(),
        }
    }
}

/// How autostart apps are launched.
#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Via {
    /// Use `systemd-run --user --scope` when a systemd user manager is
    /// reachable (so apps show up in `systemctl --user` and get their own
    /// cgroup, exactly like GNOME does); fall back to direct children.
    #[default]
    Auto,
    /// Always spawn as direct children of lion-session.
    Never,
}

/// Session-wide behavioural switches.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct SessionOptions {
    /// How long apps may take to answer `QueryEndSession` / release their
    /// inhibitors before the end is forced. Bounded on purpose: a wedged
    /// app must never be able to hang logout forever.
    pub end_timeout_ms: u64,
    /// Lock the screen when the system is about to suspend (configurable
    /// hardening: resume then requires re-authentication).
    pub lock_on_sleep: bool,
    /// Lock the screen the moment logind announces
    /// `PrepareForShutdown` (0.3.0). A shutdown can still be *cancelled*
    /// (another inhibitor, `shutdown -c`); if it is, the session is
    /// already locked instead of sitting exposed through the whole
    /// scare. GNOME locks on sleep; locking on *shutdown* is the
    /// strictly-safer superset.
    pub lock_on_shutdown: bool,
    /// Honor `/etc/xdg/autostart` and `~/.config/autostart` desktop
    /// entries (the cross-desktop autostart standard).
    pub xdg_autostart: bool,
    /// Supervise XDG autostart apps (restart on failure) instead of
    /// starting them fire-and-forget like other desktops do.
    pub supervise_xdg: bool,
    /// How autostart apps are launched.
    pub via: Via,
    /// 0.3.0 idle escalation; see `idle.rs`. Zero values (the default)
    /// keep 0.2.0 behaviour: hint forwarded, nothing escalates.
    #[serde(flatten)]
    pub idle: IdlePolicy,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            end_timeout_ms: 10_000,
            lock_on_sleep: true,
            lock_on_shutdown: true,
            xdg_autostart: true,
            supervise_xdg: false,
            via: Via::Auto,
            idle: IdlePolicy::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Grace period between announcing the end of the session (so the
    /// shell can play its fade-out) and actually tearing it down.
    pub logout_animation_ms: u64,
    pub compositor: Compositor,
    pub services: Services,
    pub session: SessionOptions,
    pub autostart: Vec<App>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub struct Raw {
    pub logout_animation_ms: Option<u64>,
    pub compositor: Option<Compositor>,
    pub services: Option<Services>,
    pub session: Option<RawSession>,
    #[serde(default)]
    pub autostart: Vec<App>,
}

/// All-Option mirror of [`SessionOptions`] so a user file can override a
/// single key without wiping the /etc layer for the others.
#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub struct RawSession {
    pub end_timeout_ms: Option<u64>,
    pub lock_on_sleep: Option<bool>,
    pub lock_on_shutdown: Option<bool>,
    pub xdg_autostart: Option<bool>,
    pub supervise_xdg: Option<bool>,
    pub via: Option<Via>,
    /// 0.3.0: `[session] lock-after-ms / logout-after-ms` (the idle
    /// policy lives under [session] because it is one knob-set among
    /// the session's behavioural switches; a separate [idle] table
    /// would be a sixth top-level section for two keys).
    pub lock_after_ms: Option<u64>,
    pub logout_after_ms: Option<u64>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            logout_animation_ms: 350,
            compositor: Compositor::default(),
            services: Services::default(),
            session: SessionOptions::default(),
            autostart: [
                ("lion-wallpaper", Phase::Display),
                ("lion-panel", Phase::Shell),
                ("lion-dock", Phase::Shell),
                ("lion-hotkeys", Phase::Shell),
                ("lion-idle", Phase::SessionServices),
                ("lion-osd", Phase::SessionServices),
            ]
            .into_iter()
            .map(|(n, p)| App::named(n, p))
            .collect(),
        }
    }
}

impl Config {
    pub fn load() -> Self {
        let mut cfg = Self::default();
        for path in Self::config_paths() {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            match toml::from_str::<Raw>(&text) {
                Ok(raw) => cfg.merge(raw),
                Err(e) => {
                    tracing::warn!(file = %path.display(), error = %e, "ignoring invalid config")
                }
            }
        }
        cfg
    }

    /// Files to read, in order. `$LION_SESSION_CONFIG` replaces the
    /// standard search (the same override convention lion-greeter uses).
    fn config_paths() -> Vec<PathBuf> {
        if let Some(p) = std::env::var_os("LION_SESSION_CONFIG") {
            return vec![PathBuf::from(p)];
        }
        let user_dir = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
        let mut v = vec![PathBuf::from("/etc/lionos/session.toml")];
        if let Some(d) = user_dir.map(|d| d.join("lionos/session.toml")) {
            v.push(d);
        }
        v
    }

    /// Scalars and tables replace outright; `[[autostart]]` entries merge
    /// by name, so a user can disable a default with just
    /// `{ name = "lion-dock", enabled = false }` instead of restating it.
    fn merge(&mut self, raw: Raw) {
        if let Some(ms) = raw.logout_animation_ms {
            self.logout_animation_ms = ms;
        }
        if let Some(c) = raw.compositor {
            self.compositor = c;
        }
        if let Some(s) = raw.services {
            self.services = s;
        }
        if let Some(s) = raw.session {
            let sess = &mut self.session;
            if let Some(v) = s.end_timeout_ms {
                sess.end_timeout_ms = v;
            }
            if let Some(v) = s.lock_on_sleep {
                sess.lock_on_sleep = v;
            }
            if let Some(v) = s.lock_on_shutdown {
                sess.lock_on_shutdown = v;
            }
            if let Some(v) = s.xdg_autostart {
                sess.xdg_autostart = v;
            }
            if let Some(v) = s.supervise_xdg {
                sess.supervise_xdg = v;
            }
            if let Some(v) = s.via {
                sess.via = v;
            }
            if let Some(v) = s.lock_after_ms {
                sess.idle.lock_after_ms = v;
            }
            if let Some(v) = s.logout_after_ms {
                sess.idle.logout_after_ms = v;
            }
        }
        for app in raw.autostart {
            match self.autostart.iter_mut().find(|a| a.name == app.name) {
                Some(existing) => *existing = app,
                None => self.autostart.push(app),
            }
        }
    }

    /// Enabled apps in start order: phase, then delay, then name.
    pub fn enabled_apps_sorted(&self) -> Vec<App> {
        let mut apps: Vec<App> = self
            .autostart
            .iter()
            .filter(|a| a.enabled)
            .cloned()
            .collect();
        apps.sort_by(|a, b| {
            a.phase
                .cmp(&b.phase)
                .then(a.delay_ms.cmp(&b.delay_ms))
                .then(a.name.cmp(&b.name))
        });
        apps
    }
}

/// `--check-config`: validate every layer that would be read, without
/// starting anything. Returns (all_ok, human report).
pub fn check_config() -> (bool, String) {
    let mut ok = true;
    let mut report = String::new();
    let paths = Config::config_paths();
    if paths.len() == 1 && std::env::var_os("LION_SESSION_CONFIG").is_some() {
        report.push_str(&format!("override: {}\n", paths[0].display()));
    }
    for path in paths {
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<Raw>(&text) {
                Ok(_) => report.push_str(&format!("ok: {}\n", path.display())),
                Err(e) => {
                    ok = false;
                    report.push_str(&format!("ERROR: {}: {e}\n", path.display()));
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                report.push_str(&format!(
                    "absent (defaults in effect): {}\n",
                    path.display()
                ));
            }
            Err(e) => {
                ok = false;
                report.push_str(&format!("ERROR: {}: {e}\n", path.display()));
            }
        }
    }
    if ok {
        let cfg = Config::load();
        report.push_str(&format!(
            "config OK: {} autostart apps, end-timeout {}ms, lock-on-sleep {}, lock-on-shutdown {}, idle lock-after {}ms logout-after {}ms\n",
            cfg.autostart.len(),
            cfg.session.end_timeout_ms,
            cfg.session.lock_on_sleep,
            cfg.session.lock_on_shutdown,
            cfg.session.idle.lock_after_ms,
            cfg.session.idle.logout_after_ms
        ));
    }
    (ok, report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_can_disable_and_add() {
        let mut c = Config::default();
        let raw: Raw = toml::from_str(
            r#"
            [[autostart]]
            name = "lion-dock"
            enabled = false
            [[autostart]]
            name = "my-app"
            command = "/opt/x"
            restart = "never"
            "#,
        )
        .unwrap();
        c.merge(raw);
        assert!(
            !c.autostart
                .iter()
                .find(|a| a.name == "lion-dock")
                .unwrap()
                .enabled
        );
        assert_eq!(c.autostart.last().unwrap().command(), "/opt/x");
    }

    #[test]
    fn session_options_merge_field_by_field() {
        let mut c = Config::default();
        let raw: Raw = toml::from_str("[session]\nend-timeout-ms = 2500\n").unwrap();
        c.merge(raw);
        assert_eq!(c.session.end_timeout_ms, 2500);
        // everything else keeps the previous layer's value
        assert!(c.session.lock_on_sleep);
        assert_eq!(c.session.via, Via::Auto);
    }

    #[test]
    fn unknown_keys_and_bad_values_do_not_wreck_the_file() {
        let raw: Result<Raw, _> = toml::from_str("[session]\nend-timeout-ms = 2500\nunknown = 1\n");
        assert!(raw.is_ok()); // unknown keys are ignored (serde default)
        let bad: Result<Raw, _> = toml::from_str("[session]\nend-timeout-ms = \"not a number\"\n");
        assert!(bad.is_err()); // but type errors are rejected (and ignored at load)
    }

    #[test]
    fn apps_sort_by_phase_then_delay_then_name() {
        let mut c = Config::default();
        let raw: Raw = toml::from_str(
            r#"
            [[autostart]]
            name = "z-app"
            phase = "display"
            [[autostart]]
            name = "a-late"
            phase = "shell"
            delay-ms = 400
            [[autostart]]
            name = "b-early"
            phase = "shell"
            delay-ms = 100
            [[autostart]]
            name = "disabled"
            phase = "display"
            enabled = false
            "#,
        )
        .unwrap();
        c.merge(raw);
        let sorted = c.enabled_apps_sorted();
        let order: Vec<&str> = sorted.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            order,
            vec![
                "lion-wallpaper", // phase 0, delay 0, name
                "z-app",          // phase 0, delay 0, name
                "lion-dock",      // phase 1, delay 0 (defaults), name
                "lion-hotkeys",
                "lion-panel",
                "b-early",   // phase 1, delay 100
                "a-late",    // phase 1, delay 400
                "lion-idle", // phase 2
                "lion-osd",
            ]
        );
    }

    #[test]
    fn phase_names_map_to_ordering_groups() {
        let raw: Raw = toml::from_str(
            "[[autostart]]\nname = \"a\"\nphase = \"session-services\"\n[[autostart]]\nname = \"b\"\nphase = \"applications\"\n",
        )
        .unwrap();
        assert_eq!(raw.autostart[0].phase, Phase::SessionServices);
        assert_eq!(raw.autostart[1].phase, Phase::Applications);
        assert!(Phase::Display < Phase::Shell);
        assert!(Phase::SessionServices < Phase::Applications);
    }

    #[test]
    fn compositor_defaults_are_recovery_safe() {
        let c = Compositor::default();
        assert!(c.restart);
        assert_eq!(c.max_restarts, 3);
        assert_eq!(c.crash_window_ms, 60_000);
    }

    #[test]
    fn via_defaults_to_auto() {
        assert_eq!(SessionOptions::default().via, Via::Auto);
    }

    #[test]
    fn idle_policy_merges_field_by_field() {
        let mut c = Config::default();
        let raw: Raw = toml::from_str("[session]\nlock-after-ms = 300_000\n").unwrap();
        c.merge(raw);
        assert_eq!(c.session.idle.lock_after_ms, 300_000);
        assert_eq!(c.session.idle.logout_after_ms, 0, "unset key untouched");
        // Second layer only sets logout: lock survives.
        let raw: Raw = toml::from_str("[session]\nlogout-after-ms = 900_000\n").unwrap();
        c.merge(raw);
        assert_eq!(c.session.idle.lock_after_ms, 300_000);
        assert_eq!(c.session.idle.logout_after_ms, 900_000);
        // Default is fully disabled (0.2.0 behaviour).
        assert!(!Config::default().session.idle.is_configured());
    }

    #[test]
    fn lock_on_shutdown_defaults_on() {
        assert!(Config::default().session.lock_on_shutdown);
        let mut c = Config::default();
        let raw: Raw = toml::from_str("[session]\nlock-on-shutdown = false\n").unwrap();
        c.merge(raw);
        assert!(!c.session.lock_on_shutdown);
    }

    #[test]
    fn per_app_resource_limits_parse() {
        let raw: Raw = toml::from_str(
            r#"
            [[autostart]]
            name = "hungry-app"
            memory-max = "512M"
            cpu-weight = 250
            tasks-max = 128
            "#,
        )
        .unwrap();
        let mut c = Config::default();
        c.merge(raw);
        let app = c.autostart.iter().find(|a| a.name == "hungry-app").unwrap();
        assert_eq!(
            app.cgroup_properties(),
            vec![
                "--property=MemoryMax=512M".to_string(),
                "--property=CPUWeight=250".to_string(),
                "--property=TasksMax=128".to_string(),
            ]
        );
        // Unconfigured apps request nothing.
        let plain = c.autostart.iter().find(|a| a.name == "lion-dock").unwrap();
        assert!(plain.cgroup_properties().is_empty());
    }
}
