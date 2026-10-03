#![forbid(unsafe_code)]
//! Unified error type for `lion-session`.
//!
//! Mirrors `lion-greeter`'s taxonomy: variants stay data-only so error
//! surfaces are stable and never leak privileged detail (no environment
//! dumps, no argv, no bus addresses of callers).

use std::fmt;

/// Top-level error for the session daemon and library.
#[derive(Debug)]
pub enum Error {
    /// Configuration could not be loaded or failed validation (fail closed).
    Config(String),
    /// An I/O error with context.
    Io(String, std::io::Error),
    /// The systemd user-manager (D-Bus or systemctl fallback) failed.
    Systemd(String),
    /// The logind (D-Bus) backend failed.
    Logind(String),
    /// The D-Bus service layer failed (name lost, marshalling, bus gone).
    Bus(String),
    /// Request denied by policy (authorization, rate limit, bad state).
    Denied(String),
    /// Service supervision failure (spawn failure, crash-loop).
    Service(String),
    /// Invalid input from a caller (bad inhibitor `what`, bad app_id…).
    InvalidParams(String),
    /// Anything else.
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Config(m) => write!(f, "config error: {m}"),
            Error::Io(ctx, e) => write!(f, "io error ({ctx}): {e}"),
            Error::Systemd(m) => write!(f, "systemd backend error: {m}"),
            Error::Logind(m) => write!(f, "logind backend error: {m}"),
            Error::Bus(m) => write!(f, "dbus error: {m}"),
            Error::Denied(m) => write!(f, "denied: {m}"),
            Error::Service(m) => write!(f, "service error: {m}"),
            Error::InvalidParams(m) => write!(f, "invalid params: {m}"),
            Error::Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io("unspecified".into(), e)
    }
}

/// Convenience result alias.
pub type Result<T> = std::result::Result<T, Error>;
