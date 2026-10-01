//! Best-effort helpers around systemd / D-Bus tooling. Failures are logged,
//! never fatal: a session without them still has to work.

use std::{process::Stdio, time::Duration};
use tokio::{process::Command, time::timeout};

pub async fn run_quiet(cmd: &str, args: &[&str]) {
    let fut = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .status();
    match timeout(Duration::from_secs(5), fut).await {
        Ok(Ok(s)) if s.success() => tracing::debug!(cmd, ?args, "ok"),
        Ok(Ok(s)) => tracing::warn!(cmd, ?args, status = %s, "exited non-zero"),
        Ok(Err(e)) => tracing::warn!(cmd, error = %e, "could not run"),
        Err(_) => tracing::warn!(cmd, "timed out"),
    }
}

/// Push the session environment to D-Bus-activated services and to
/// `systemd --user`, so both see WAYLAND_DISPLAY / DBUS_SESSION_BUS_ADDRESS
/// / XDG_* without needing PAM to have set them.
pub async fn import_activation_env() {
    let mut args = vec!["--systemd"];
    args.extend(crate::env::EXPORTED.iter().copied());
    run_quiet("dbus-update-activation-environment", &args).await;
}

/// True when a `systemd --user` manager is reachable (autostart apps can
/// then be launched in their own scopes, GNOME-style). Probed once at
/// startup; a false result only means "direct children", never an error.
pub async fn systemd_user_available() -> bool {
    match timeout(
        Duration::from_secs(2),
        Command::new("systemctl")
            .args(["--user", "show-environment"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
    )
    .await
    {
        Ok(Ok(s)) => s.success(),
        _ => false,
    }
}

pub async fn user_target(action: &str) {
    run_quiet("systemctl", &["--user", action, "lion-session.target"]).await;
}
