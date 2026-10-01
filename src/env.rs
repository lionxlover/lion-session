//! Session environment. Runs single-threaded, before the async runtime,
//! because `std::env::set_var` is only sound with no other threads around.
//! Everything set here is inherited by the compositor and every autostart
//! app.
//!
//! The runtime directory is validated, not just checked for existence:
//! logind guarantees `$XDG_RUNTIME_DIR` is owned by the user with mode
//! 0700, and anything else is a session-hijack vector (a group-readable
//! runtime dir leaks the Wayland socket -- and with it the entire
//! desktop -- to every process in that group).

use anyhow::{bail, Context, Result};
use std::{env, path::PathBuf};
use users::os::unix::UserExt;

pub struct Paths {
    pub runtime_dir: PathBuf,
    pub wayland_socket: PathBuf,
}

fn set_default(key: &str, val: impl AsRef<std::ffi::OsStr>) {
    if env::var_os(key).is_none() {
        env::set_var(key, val);
    }
}

/// Pure decision core of the runtime-dir check, unit-testable.
/// Returns Err(reason) when the directory must be refused.
pub fn runtime_dir_check(uid: u32, owner: u32, mode: u32) -> Result<()> {
    if owner != uid {
        bail!("owned by uid {owner}, expected {uid} (spoofed runtime dir?)");
    }
    if mode & 0o077 != 0 {
        bail!(
            "mode {:o} is group/world accessible; logind guarantees 0700 \
             (a shared runtime dir leaks the Wayland socket)",
            mode & 0o777
        );
    }
    Ok(())
}

pub fn prepare(expected_user: Option<&str>, wayland_display: &str) -> Result<Paths> {
    let uid = unsafe { libc::geteuid() };
    if uid == 0 {
        bail!("refusing to run a desktop session as root");
    }
    let me = users::get_user_by_uid(uid).context("current uid has no passwd entry")?;
    let name = me.name().to_string_lossy().into_owned();
    if let Some(want) = expected_user {
        if want != name {
            bail!("started as {name} but asked to run the session for {want}");
        }
    }
    let home = me.home_dir().to_path_buf();

    set_default("USER", &name);
    set_default("LOGNAME", &name);
    set_default("HOME", &home);
    set_default("SHELL", me.shell());

    let runtime = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{uid}")));
    if !runtime.is_dir() {
        bail!(
            "{} does not exist (is the logind session set up?)",
            runtime.display()
        );
    }
    validate_runtime_dir(&runtime, uid)?;
    env::set_var("XDG_RUNTIME_DIR", &runtime);

    set_default("XDG_CONFIG_HOME", home.join(".config"));
    set_default("XDG_DATA_HOME", home.join(".local/share"));
    set_default("XDG_STATE_HOME", home.join(".local/state"));
    set_default("XDG_CACHE_HOME", home.join(".cache"));
    set_default("XDG_DATA_DIRS", "/usr/local/share:/usr/share");
    set_default("XDG_CONFIG_DIRS", "/etc/xdg");

    // These are the session's identity; always ours, never inherited.
    env::set_var("XDG_SESSION_TYPE", "wayland");
    env::set_var("XDG_CURRENT_DESKTOP", "LionOS");
    env::set_var("XDG_SESSION_DESKTOP", "LionOS");
    env::set_var("WAYLAND_DISPLAY", wayland_display);

    // Nudge toolkits toward Wayland, with X11/XWayland fallback where
    // supported, without forcing it and breaking apps that need XWayland.
    set_default("GDK_BACKEND", "wayland,x11");
    set_default("QT_QPA_PLATFORM", "wayland;xcb");
    set_default("MOZ_ENABLE_WAYLAND", "1");
    set_default("ELECTRON_OZONE_PLATFORM_HINT", "auto");
    set_default("_JAVA_AWT_WM_NONREPARENTING", "1");

    let local_bin = home.join(".local/bin");
    if local_bin.is_dir() {
        let path = env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into());
        env::set_var("PATH", format!("{}:{path}", local_bin.display()));
    }

    Ok(Paths {
        wayland_socket: runtime.join(wayland_display),
        runtime_dir: runtime,
    })
}

/// Enforce the logind contract on the runtime dir: right owner, 0700.
fn validate_runtime_dir(dir: &std::path::Path, uid: u32) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(dir).with_context(|| format!("could not stat {}", dir.display()))?;
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        md.permissions().mode()
    };
    runtime_dir_check(uid, md.uid(), mode)
        .with_context(|| format!("bad runtime dir {}", dir.display()))
}

/// Variables handed to the D-Bus activation environment and to
/// `systemctl --user import-environment`, so D-Bus-activated services and
/// systemd --user units see the same session.
pub const EXPORTED: &[&str] = &[
    "WAYLAND_DISPLAY",
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_CURRENT_DESKTOP",
    "XDG_SESSION_TYPE",
    "XDG_SESSION_DESKTOP",
    "XDG_RUNTIME_DIR",
    "XDG_DATA_DIRS",
    "GDK_BACKEND",
    "QT_QPA_PLATFORM",
    "MOZ_ENABLE_WAYLAND",
    "ELECTRON_OZONE_PLATFORM_HINT",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_dir_rejects_wrong_owner() {
        assert!(runtime_dir_check(1000, 0, 0o700).is_err());
        assert!(runtime_dir_check(1000, 1001, 0o700).is_err());
        assert!(runtime_dir_check(1000, 1000, 0o700).is_ok());
    }

    #[test]
    fn runtime_dir_rejects_group_or_world_access() {
        assert!(runtime_dir_check(1000, 1000, 0o750).is_err());
        assert!(runtime_dir_check(1000, 1000, 0o707).is_err());
        assert!(runtime_dir_check(1000, 1000, 0o777).is_err());
        // setuid/sticky bits are irrelevant to the check
        assert!(runtime_dir_check(1000, 1000, 0o1700).is_ok());
        assert!(runtime_dir_check(1000, 1000, 0o4700).is_ok());
    }

    #[test]
    fn runtime_dir_check_error_names_the_problem() {
        let err = runtime_dir_check(1000, 0, 0o777).unwrap_err().to_string();
        assert!(err.contains("owned by uid 0"), "got: {err}");
        let err = runtime_dir_check(1000, 1000, 0o755)
            .unwrap_err()
            .to_string();
        assert!(err.contains("leaks the Wayland socket"), "got: {err}");
    }
}
