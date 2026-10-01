//! lion-session: owns the LionOS user session.
//!
//! Started by `lion-greeter` (already authenticated, already the target
//! user, already inside a logind session). It:
//!   - sets up the XDG / Wayland / session-bus environment
//!   - spawns the compositor and the configured + XDG autostart apps
//!   - supervises both: restart policies, crash-loop guards, backoff
//!     with anti-thundering-herd jitter, and compositor crash recovery
//!   - exposes Logout / Restart / Shutdown / Suspend / Hibernate / Lock on
//!     `os.lionos.Session`, plus the cooperative end protocol
//!     (RegisterClient / QueryEndSession / EndSessionResponse / Inhibit)
//!   - escalates idle into lock/logout per policy (0.3.0) and locks on
//!     imminent shutdown (0.3.0)
//!   - forwards Lock to `lion-locker` and power actions to `lion-power`,
//!     and integrates with logind (delay inhibitors, sleep/shutdown
//!     events, idle hints, loginctl lock-session)
//!   - hardens itself: non-dumpable, no core dumps (0.3.0), and its
//!     direct children carry PDEATHSIG so they cannot outlive it
//!
//! Not its job: rendering (compositor), authentication (greeter),
//! input-device-level idle detection (lion-idle reports idle *to* us).

use anyhow::Result;
use lion_session::{config, env, harden, service, session};
use std::process::ExitCode;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lion_session=info".into()),
        )
        .with_ansi(false)
        .init();

    // 0.3.0: process hardening first — non-dumpable + no core dumps,
    // best-effort and logged (see harden.rs for why NO_NEW_PRIVS is
    // deliberately absent).
    let hardening = harden::apply();

    match run(hardening) {
        Ok(code) => code,
        Err(e) => {
            tracing::error!("fatal: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(hardening: harden::HardenReport) -> Result<ExitCode> {
    let mut user = None;
    let mut session_id = None;
    let mut check_config = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--user" => user = args.next(),
            // 0.3.0: chosen by lion-greeter from the XDG session list
            // (wayland-sessions/x11-sessions). lion-session records it
            // (XDG_SESSION_DESKTOP semantics) — the compositor command
            // it launches stays configured here, because "which
            // session manager owns the desktop" is *this* file's job.
            "--session" => session_id = args.next(),
            "--check-config" => check_config = true,
            "--version" => {
                println!(
                    "lion-session {} (LionOS user session daemon)\n\
                     Edition 2021\n\
                     Hardening: {}\n\
                     Features: {}",
                    env!("CARGO_PKG_VERSION"),
                    hardening.tag(),
                    service::CAPABILITIES.join(" ")
                );
                return Ok(ExitCode::SUCCESS);
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
    }

    if check_config {
        let (ok, report) = config::check_config();
        print!("{report}");
        return Ok(if ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        });
    }

    if let Some(id) = session_id.as_deref() {
        // The greeter validated the id against the XDG session list
        // before sending it; record it for anything downstream that
        // wants to know which desktop the user chose. Refuse nothing:
        // an odd id is a data bug, not a login blocker.
        tracing::info!(session = id, "selected desktop session");
    }

    let cfg = config::Config::load();
    // Environment must be prepared before any threads exist (std::env::set_var
    // is only sound single-threaded), so this runs before building the runtime.
    let paths = env::prepare(user.as_deref(), &cfg.compositor.wayland_display)?;

    // 0.3.0: a bounded runtime. The 0.2.0 default was `num_cpus()`
    // worker threads (16 on a typical desktop) for a daemon whose
    // async surface is one D-Bus service and a handful of child
    // watchers — 2 workers + a small blocking pool does the same work
    // with fewer threads to spawn at startup and less idle RSS.
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(4)
        .enable_all()
        .build()?
        .block_on(session::run(cfg, paths))
}
