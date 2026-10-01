//! Session D-Bus bus. On Debian with `dbus-user-session`, the bus is
//! socket-activated at $XDG_RUNTIME_DIR/bus and already reachable; we use
//! it as-is. Otherwise we start (and, on teardown, stop) our own
//! `dbus-daemon --session`.

use anyhow::{bail, Result};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{net::UnixStream, process::Child, time::sleep};

pub struct SessionBus {
    /// `Some` only if we started our own daemon and must stop it ourselves.
    child: Option<Child>,
}

impl SessionBus {
    pub async fn shutdown(&mut self) {
        if let Some(c) = self.child.as_mut() {
            crate::proc::terminate(c, Duration::from_secs(2)).await;
        }
    }
}

/// Extract the socket path from `unix:path=/x,guid=y`. Returns `None` for
/// other address kinds (abstract, tcp), which we simply trust as-is.
fn socket_path(addr: &str) -> Option<PathBuf> {
    let rest = addr.strip_prefix("unix:")?;
    rest.split(',')
        .find_map(|kv| kv.strip_prefix("path="))
        .map(PathBuf::from)
}

async fn reachable(sock: &Path) -> bool {
    UnixStream::connect(sock).await.is_ok()
}

pub async fn ensure(runtime_dir: &Path) -> Result<SessionBus> {
    if let Ok(addr) = std::env::var("DBUS_SESSION_BUS_ADDRESS") {
        match socket_path(&addr) {
            Some(p) if !reachable(&p).await => {
                tracing::warn!(%addr, "inherited bus address unreachable, replacing");
            }
            _ => return Ok(SessionBus { child: None }),
        }
    }

    let sock = runtime_dir.join("bus");
    let addr = format!("unix:path={}", sock.display());

    if sock.exists() {
        if reachable(&sock).await {
            std::env::set_var("DBUS_SESSION_BUS_ADDRESS", &addr);
            return Ok(SessionBus { child: None });
        }
        let _ = std::fs::remove_file(&sock); // stale socket from a crash
    }

    let args = vec![
        "--session".into(),
        "--nofork".into(),
        "--nopidfile".into(),
        format!("--address={addr}"),
    ];
    let child = crate::proc::spawn("dbus-daemon", &args)
        .map_err(|e| anyhow::anyhow!("could not start dbus-daemon: {e}"))?;
    let mut bus = SessionBus { child: Some(child) };

    for _ in 0..100 {
        if reachable(&sock).await {
            std::env::set_var("DBUS_SESSION_BUS_ADDRESS", &addr);
            tracing::info!(%addr, "started session bus");
            return Ok(bus);
        }
        sleep(Duration::from_millis(50)).await;
    }
    bus.shutdown().await;
    bail!("session bus did not come up in time")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_unix_path_address() {
        assert_eq!(
            socket_path("unix:path=/run/user/1000/bus,guid=abc"),
            Some(PathBuf::from("/run/user/1000/bus"))
        );
        assert_eq!(socket_path("unix:abstract=/tmp/x"), None);
    }
}
