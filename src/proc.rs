//! Child process management: each child gets its own process group so the
//! whole tree can be signalled together, and is torn down politely
//! (SIGTERM, then SIGKILL after a grace period).
//!
//! Autostart apps can optionally be launched through
//! `systemd-run --user --scope` so they get their own cgroup and become
//! visible/controllable in `systemctl --user` (the same integration GNOME
//! uses); the session falls back to direct children whenever no user
//! manager is reachable.

use crate::{
    config::{App, RestartPolicy},
    harden::jitter_ms,
    metrics::{Counter, Metrics},
};
use std::{
    collections::VecDeque,
    io,
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    process::{Child, Command},
    sync::watch,
    time::{sleep, timeout},
};

pub const GRACE: Duration = Duration::from_secs(3);
const CRASH_WINDOW: Duration = Duration::from_secs(60);
const MAX_STARTS_IN_WINDOW: usize = 5;
/// Restart backoff jitter bound (0.3.0): when one display hiccup kills
/// several apps in the same millisecond, their restarts must not also
/// align. See `harden::jitter_ms`.
const RESTART_JITTER_MS: u64 = 150;

/// How an app should be spawned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launcher {
    /// Direct child of lion-session (own process group, supervised here).
    Direct,
    /// `systemd-run --user --scope --collect`: own cgroup, visible in
    /// `systemctl --user`, still supervised here (the wrapper forwards
    /// signals and passes the exit status through).
    SystemdScope,
}

pub fn spawn(cmd: &str, args: &[String]) -> io::Result<Child> {
    let mut cmd = Command::new(cmd);
    cmd.args(args)
        .stdin(Stdio::null())
        .process_group(0)
        .kill_on_drop(true);
    // SAFETY: pre_exec runs between fork and exec; only async-signal-safe
    // libc calls. PR_SET_PDEATHSIG(SIGTERM) makes this child clean itself
    // up if the session manager dies outright (SIGKILL, OOM): without it,
    // the direct-launch fallback leaks orphaned apps onto the user's
    // next session — GNOME avoids this via systemd cgroups, we get the
    // same guarantee for the fallback path with one prctl. The signal
    // fires on parent-thread exit; tokio worker threads live until
    // runtime shutdown, and at that point tearing the children down is
    // exactly the intent.
    unsafe {
        cmd.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                // Not fatal: best-effort cleanup only.
                return Ok(());
            }
            Ok(())
        });
    }
    cmd.spawn()
}

/// Spawn an autostart app per the chosen launcher. For scopes, a fresh
/// unit name is used every start so restarts never collide with a lingering
/// scope of the same name. 0.3.0: per-app cgroup resource limits
/// (`memory-max` / `cpu-weight` / `tasks-max` in session.toml) ride along
/// as `--property=` arguments — a policy GNOME needs hand-written units
/// for, expressed here as three config keys.
pub fn spawn_app(launcher: Launcher, app: &App, start_index: u64) -> io::Result<Child> {
    match launcher {
        Launcher::Direct => {
            if !app.cgroup_properties().is_empty() {
                // Log once per start, not per spawn-attempt: keep the
                // journal readable while the operator learns why the
                // limits are inert.
                tracing::warn!(
                    app = %app.name,
                    "resource limits configured but systemd is unavailable; running unlimited"
                );
            }
            spawn(app.command(), &app.args)
        }
        Launcher::SystemdScope => {
            let unit = format!("lion-app-{}-{}.scope", slug(&app.name), start_index);
            let mut args: Vec<String> = vec![
                "--user".into(),
                "--scope".into(),
                "--collect".into(),
                format!("--unit={unit}"),
            ];
            args.extend(app.cgroup_properties());
            // systemd-run does PATH-search the command, but an absolute
            // path is unambiguous when the app's argv must be split.
            args.push(app.command().to_string());
            args.extend(app.args.iter().cloned());
            spawn("systemd-run", &args)
        }
    }
}

fn executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// `[a-zA-Z0-9_.-]` only, for unit names.
fn slug(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

pub fn signal_group(child: &Child, sig: i32) {
    if let Some(pid) = child.id() {
        // SAFETY: plain syscall on our own child's pgid; a stale pgid
        // (already reaped) just yields ESRCH, which we ignore.
        unsafe { libc::killpg(pid as i32, sig) };
    }
}

pub async fn terminate(child: &mut Child, grace: Duration) {
    if child.id().is_none() {
        return; // already reaped
    }
    signal_group(child, libc::SIGTERM);
    if timeout(grace, child.wait()).await.is_err() {
        tracing::warn!("child ignored SIGTERM, sending SIGKILL");
        signal_group(child, libc::SIGKILL);
        let _ = child.wait().await;
    }
}

pub fn in_path(cmd: &str) -> bool {
    if cmd.contains('/') {
        return executable(Path::new(cmd));
    }
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| executable(&d.join(cmd))))
        .unwrap_or(false)
}

/// Run one autostart app until shutdown, restarting per its policy with
/// backoff, and giving up if it's crash-looping. Metrics record first
/// starts, restarts, and crash-loop give-ups.
pub async fn run_app(
    app: App,
    mut shutdown: watch::Receiver<bool>,
    launcher: Launcher,
    metrics: std::sync::Arc<Metrics>,
) {
    if app.delay_ms > 0 {
        tokio::select! {
            _ = sleep(Duration::from_millis(app.delay_ms)) => {}
            _ = shutdown.changed() => return,
        }
    }
    if !in_path(app.command()) {
        tracing::info!(app = %app.name, "not installed, skipping");
        return;
    }

    let mut recent_starts: VecDeque<Instant> = VecDeque::new();
    let mut start_index: u64 = 0;
    loop {
        if *shutdown.borrow() {
            return;
        }
        let mut child = match spawn_app(launcher, &app, start_index) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(app = %app.name, error = %e, "failed to start");
                return;
            }
        };
        if start_index == 0 {
            metrics.inc(Counter::AppsStarted);
        } else {
            metrics.inc(Counter::AppRestarts);
        }
        start_index += 1;
        tracing::info!(app = %app.name, pid = ?child.id(), ?launcher, "started");

        let status = tokio::select! {
            s = child.wait() => s,
            _ = shutdown.changed() => {
                terminate(&mut child, GRACE).await;
                return;
            }
        };
        let succeeded = status.as_ref().map(|s| s.success()).unwrap_or(false);
        tracing::info!(app = %app.name, ?status, "exited");

        let should_restart = match app.restart {
            RestartPolicy::Never => false,
            RestartPolicy::OnFailure => !succeeded,
            RestartPolicy::Always => true,
        };
        if !should_restart {
            return;
        }

        let now = Instant::now();
        recent_starts.push_back(now);
        while recent_starts
            .front()
            .is_some_and(|t| now.duration_since(*t) > CRASH_WINDOW)
        {
            recent_starts.pop_front();
        }
        if recent_starts.len() > MAX_STARTS_IN_WINDOW {
            metrics.inc(Counter::AppCrashloops);
            tracing::error!(app = %app.name, "crash-looping, giving up");
            return;
        }
        let backoff =
            Duration::from_millis(500 * recent_starts.len() as u64 + jitter_ms(RESTART_JITTER_MS));
        tokio::select! {
            _ = sleep(backoff) => {}
            _ = shutdown.changed() => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_sanitizes_unit_names() {
        assert_eq!(slug("lion-dock"), "lion-dock");
        assert_eq!(slug("My App/2"), "My-App-2");
        assert_eq!(slug("a::b"), "a--b");
    }

    #[test]
    fn in_path_finds_ls_and_rejects_nonsense() {
        assert!(in_path("ls"));
        assert!(!in_path("definitely-not-a-command-xyz"));
        assert!(!in_path("/nonexistent/absolute/path"));
    }

    /// The direct-launch fallback sets PR_SET_PDEATHSIG(SIGTERM): the
    /// child must die when the thread that spawned it exits. Proven
    /// with a real child spawned on a helper thread that then exits:
    /// after the thread joins, the child is gone (or zombified —
    /// nobody reaps it, which still proves the signal fired).
    #[test]
    fn pdeathsig_fires_when_spawning_thread_exits() {
        use std::os::unix::process::CommandExt;
        use std::process::Command as StdCommand;
        let pid = std::thread::spawn(|| -> u32 {
            let mut cmd = StdCommand::new("sleep");
            cmd.arg("30").stdin(Stdio::null()).stdout(Stdio::null());
            // SAFETY: identical pre_exec to the production spawn().
            unsafe {
                cmd.pre_exec(|| {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                    Ok(())
                });
            }
            let child = cmd.spawn().expect("spawn sleep");
            let pid = child.id();
            // Deliberately leak the handle: kill-on-drop would mask the
            // mechanism under test.
            std::mem::forget(child);
            pid
        })
        .join()
        .expect("spawning thread panicked");
        // Child existed while the thread lived.
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, 0);
        // Thread exit => SIGTERM delivered => not running anymore.
        std::thread::sleep(Duration::from_millis(400));
        let running = unsafe { libc::kill(pid as i32, 0) == 0 }
            && std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .map(|s| {
                    s.rsplit(')')
                        .next()
                        .unwrap_or("")
                        .trim_start()
                        .starts_with('S')
                })
                .unwrap_or(false);
        assert!(!running, "child outlived its PDEATHSIG parent thread");
    }
}
