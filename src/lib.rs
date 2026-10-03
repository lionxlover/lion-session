//! `lion-session` — LionOS per-user session supervisor (spec 02).
//!
//! Starts and supervises the compositor and every shell service, owns the
//! session lifecycle (logout/restart/shutdown/suspend/hibernate/lock/
//! switch-user, inhibitors respected), and executes those actions through
//! logind. Bus service `os.lionos.Session1` on the session bus.
//!
//! Unsafe policy (spec 02 §8): every module carries
//! `#![forbid(unsafe_code)]` EXCEPT the single audited FFI module
//! `sysffi` (pidfd), which is `#![allow(unsafe_code)]` with a documented
//! audit contract — mirroring lion-greeter's convention.

pub mod authz;
pub mod autostart;
pub mod backends;
pub mod cli;
pub mod config;
pub mod environment;
pub mod error;
pub mod inhibitors;
pub mod lifecycle;
pub mod mocks;
pub mod notify;
pub mod ports;
pub mod resolver;
pub mod safemode;
pub mod savestate;
pub mod session;
pub mod supervisor;
pub mod sysffi;
pub mod throttle;

#[cfg(feature = "real-backends")]
pub mod bus;

pub use config::{Config, DEFAULT_CONFIG_PATH, SCHEMA_JSON};
pub use error::{Error, Result};

/// Crate version (reported by `--version`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Spec number this crate implements.
pub const SPEC: &str = "02";
