#![forbid(unsafe_code)]
//! Session environment construction (spec 02 §3 startup orchestration):
//! build the XDG/Wayland/D-Bus/locale/theme environment, keep it
//! importable into systemd --user and D-Bus activation, and bound every
//! value (names and sizes) before it reaches any child process.
//!
//! Layering (later layers win on conflict):
//! 1. imported variables from the launching environment (greeter/systemd
//!    user instance: locale, TZ…), filtered by the configured allowlist;
//! 2. session-standard variables (XDG_CURRENT_DESKTOP, session type,
//!    Wayland/D-Bus addresses…);
//! 3. theme variables;
//! 4. config `environment.extra` (explicit overrides, applied last).

use std::collections::BTreeMap;

/// Max imported pairs / total pairs (bound every input, spec §8).
pub const MAX_PAIRS: usize = 128;
pub const MAX_NAME_LEN: usize = 64;
pub const MAX_VALUE_LEN: usize = 8192;

/// Inputs to environment construction.
#[derive(Debug, Clone, Default)]
pub struct EnvInputs {
    /// Filtered imports from the launching environment (already validated
    /// by the caller with [`import_from_env`]).
    pub imports: Vec<(String, String)>,
    /// $XDG_RUNTIME_DIR (required, absolute).
    pub xdg_runtime_dir: String,
    /// Wayland socket basename once the compositor is up.
    pub wayland_display: Option<String>,
    /// Desktop id (config `session.desktop_name`).
    pub desktop_name: String,
    /// D-Bus session address override; default derived from the runtime dir.
    pub dbus_session_bus_address: Option<String>,
    /// Theme variables (Leonux design language: LEONUX_THEME, …).
    pub theme: BTreeMap<String, String>,
    /// Config `environment.extra` overrides.
    pub extra: BTreeMap<String, String>,
}

/// Validate one variable name.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name
            .chars()
            .next()
            .map(|c| c.is_ascii_alphabetic() || c == '_')
            .unwrap_or(false)
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Validate one value (no NULs, bounded).
pub fn valid_value(value: &str) -> bool {
    !value.contains('\0') && value.len() <= MAX_VALUE_LEN
}

/// Pull configured imports out of a launching environment map.
pub fn import_from_env(
    source: &BTreeMap<String, String>,
    allowlist: &[String],
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for key in allowlist {
        if let Some(v) = source.get(key) {
            if valid_name(key) && valid_value(v) && out.len() < MAX_PAIRS {
                out.push((key.clone(), v.clone()));
            }
        }
    }
    out
}

/// The constructed session environment.
#[derive(Debug, Clone, Default)]
pub struct SessionEnv {
    pairs: Vec<(String, String)>,
}

impl SessionEnv {
    /// Build from inputs (validating everything).
    pub fn build(inputs: &EnvInputs) -> Result<SessionEnv, String> {
        if inputs.xdg_runtime_dir.is_empty() {
            return Err("xdg_runtime_dir is required".into());
        }
        if !inputs.xdg_runtime_dir.starts_with('/') {
            return Err("xdg_runtime_dir must be an absolute path".into());
        }
        if !valid_value(&inputs.xdg_runtime_dir) {
            return Err("xdg_runtime_dir is not a valid value".into());
        }

        let mut pairs: Vec<(String, String)> = Vec::new();
        let push = |k: String, v: String, pairs: &mut Vec<(String, String)>| {
            if !valid_name(&k) {
                return; // config typo: skip (import_from_env pre-filters)
            }
            if !valid_value(&v) {
                return;
            }
            if pairs.len() >= MAX_PAIRS {
                return;
            }
            // last-wins within the pair list too
            if let Some(existing) = pairs.iter_mut().find(|(ek, _)| *ek == k) {
                existing.1 = v;
            } else {
                pairs.push((k, v));
            }
        };

        for (k, v) in &inputs.imports {
            push(k.clone(), v.clone(), &mut pairs);
        }
        push(
            "XDG_CURRENT_DESKTOP".into(),
            inputs.desktop_name.clone(),
            &mut pairs,
        );
        push("XDG_SESSION_DESKTOP".into(), "lionos".into(), &mut pairs);
        push("XDG_SESSION_TYPE".into(), "wayland".into(), &mut pairs);
        push(
            "XDG_RUNTIME_DIR".into(),
            inputs.xdg_runtime_dir.clone(),
            &mut pairs,
        );
        if let Some(wl) = &inputs.wayland_display {
            push("WAYLAND_DISPLAY".into(), wl.clone(), &mut pairs);
        }
        match &inputs.dbus_session_bus_address {
            Some(addr) => push("DBUS_SESSION_BUS_ADDRESS".into(), addr.clone(), &mut pairs),
            None => push(
                "DBUS_SESSION_BUS_ADDRESS".into(),
                format!("unix:path={}/bus", inputs.xdg_runtime_dir),
                &mut pairs,
            ),
        }
        for (k, v) in &inputs.theme {
            push(k.clone(), v.clone(), &mut pairs);
        }
        for (k, v) in &inputs.extra {
            push(k.clone(), v.clone(), &mut pairs);
        }
        Ok(SessionEnv { pairs })
    }

    /// Lookup.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// All pairs in precedence order (first occurrence wins when a map
    /// is built without override semantics).
    pub fn pairs(&self) -> &[(String, String)] {
        &self.pairs
    }

    /// A map for `Command::envs` (BTreeMap: last insertion wins, so build
    /// in order — equal keys were deduped at build time).
    pub fn to_env_map(&self) -> BTreeMap<String, String> {
        self.pairs.iter().cloned().collect()
    }

    /// `systemctl --user import-environment` style list ("KEY=VALUE").
    pub fn systemd_import_list(&self) -> Vec<String> {
        self.pairs.iter().map(|(k, v)| format!("{k}={v}")).collect()
    }

    /// Full environment for directly spawned children: the session pairs
    /// layered over a clean minimal base (PATH, HOME).
    pub fn child_env(&self) -> Vec<(String, String)> {
        let mut env: Vec<(String, String)> = Vec::new();
        if let Some(home) = std::env::var_os("HOME") {
            env.push(("HOME".into(), home.to_string_lossy().to_string()));
        }
        if let Ok(path) = std::env::var("PATH") {
            env.push(("PATH".into(), path));
        }
        for (k, v) in &self.pairs {
            if let Some(existing) = env.iter_mut().find(|(ek, _)| ek == k) {
                existing.1 = v.clone();
            } else {
                env.push((k.clone(), v.clone()));
            }
        }
        env
    }

    /// Path of the compositor's Wayland socket.
    pub fn wayland_socket_path(&self) -> Option<String> {
        let runtime = self.get("XDG_RUNTIME_DIR")?;
        let display = self.get("WAYLAND_DISPLAY")?;
        Some(format!("{runtime}/{display}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> EnvInputs {
        EnvInputs {
            imports: vec![("LANG".into(), "en_US.UTF-8".into())],
            xdg_runtime_dir: "/run/user/1000".into(),
            wayland_display: Some("wayland-0".into()),
            desktop_name: "LionOS".into(),
            dbus_session_bus_address: None,
            theme: [("LEONUX_THEME".to_string(), "leonux-dark".into())]
                .into_iter()
                .collect(),
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn standard_vars_present() {
        let e = SessionEnv::build(&inputs()).unwrap();
        assert_eq!(e.get("XDG_CURRENT_DESKTOP"), Some("LionOS"));
        assert_eq!(e.get("XDG_SESSION_TYPE"), Some("wayland"));
        assert_eq!(e.get("XDG_SESSION_DESKTOP"), Some("lionos"));
        assert_eq!(e.get("WAYLAND_DISPLAY"), Some("wayland-0"));
        assert_eq!(
            e.get("DBUS_SESSION_BUS_ADDRESS"),
            Some("unix:path=/run/user/1000/bus")
        );
        assert_eq!(e.get("LANG"), Some("en_US.UTF-8"));
        assert_eq!(e.get("LEONUX_THEME"), Some("leonux-dark"));
    }

    #[test]
    fn dbus_address_override() {
        let mut i = inputs();
        i.dbus_session_bus_address = Some("unix:abstract=/tmp/dbus-xyz".into());
        let e = SessionEnv::build(&i).unwrap();
        assert_eq!(
            e.get("DBUS_SESSION_BUS_ADDRESS"),
            Some("unix:abstract=/tmp/dbus-xyz")
        );
    }

    #[test]
    fn extra_overrides_standard() {
        let mut i = inputs();
        i.extra
            .insert("XDG_CURRENT_DESKTOP".into(), "LionOS-Dev".into());
        let e = SessionEnv::build(&i).unwrap();
        assert_eq!(e.get("XDG_CURRENT_DESKTOP"), Some("LionOS-Dev"));
    }

    #[test]
    fn runtime_dir_required_absolute() {
        let mut i = inputs();
        i.xdg_runtime_dir = String::new();
        assert!(SessionEnv::build(&i).is_err());
        i.xdg_runtime_dir = "relative/path".into();
        assert!(SessionEnv::build(&i).is_err());
    }

    #[test]
    fn wayland_display_optional() {
        let mut i = inputs();
        i.wayland_display = None;
        let e = SessionEnv::build(&i).unwrap();
        assert_eq!(e.get("WAYLAND_DISPLAY"), None);
        assert_eq!(e.wayland_socket_path(), None);
    }

    #[test]
    fn socket_path_computed() {
        let e = SessionEnv::build(&inputs()).unwrap();
        assert_eq!(
            e.wayland_socket_path().as_deref(),
            Some("/run/user/1000/wayland-0")
        );
    }

    #[test]
    fn name_validation() {
        assert!(valid_name("XDG_CURRENT_DESKTOP"));
        assert!(valid_name("_private"));
        assert!(!valid_name(""));
        assert!(!valid_name("3invalid"));
        assert!(!valid_name("has-dash"));
        assert!(!valid_name("has space"));
        assert!(!valid_name(&"x".repeat(65)));
    }

    #[test]
    fn value_validation() {
        assert!(valid_value("hello"));
        assert!(!valid_value("with\0nul"));
        assert!(!valid_value(&"x".repeat(8193)));
    }

    #[test]
    fn import_filtering() {
        let mut src = BTreeMap::new();
        src.insert("LANG".into(), "C".into());
        src.insert("SECRET".into(), "hunter2".into());
        let out = import_from_env(&src, &["LANG".to_string()]);
        assert_eq!(out, vec![("LANG".to_string(), "C".to_string())]);
    }

    #[test]
    fn systemd_import_list_format() {
        let e = SessionEnv::build(&inputs()).unwrap();
        let list = e.systemd_import_list();
        assert!(list.contains(&"XDG_CURRENT_DESKTOP=LionOS".to_string()));
        assert!(list.iter().all(|kv| {
            let (k, _) = kv.split_once('=').unwrap();
            valid_name(k)
        }));
    }

    #[test]
    fn child_env_layers_session_over_base() {
        let e = SessionEnv::build(&inputs()).unwrap();
        let env = e.child_env();
        // PATH present from base
        assert!(env.iter().any(|(k, _)| k == "PATH"));
        // session vars present
        assert!(env
            .iter()
            .any(|(k, v)| k == "XDG_CURRENT_DESKTOP" && v == "LionOS"));
    }
}
