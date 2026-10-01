//! lion-session: the LionOS user session manager.
//!
//! The binary is a thin CLI over this library (0.3.0): the split exists
//! so the config layering, XDG desktop-entry parsing and idle-escalation
//! policy are integration- and fuzz-testable without spawning a session.
//! See `tests/` for both layers (integration against the real binary,
//! fuzz against the library parsers).
//!
//! Module map: [`session`] orchestrates; [`bus`]/[`env`]/[`sys`] build
//! the environment; [`proc`] supervises children; [`desktop`] loads XDG
//! autostart; [`inhibit`] runs the cooperative end protocol; [`logind`]
//! bridges system events; [`service`] is the D-Bus surface; [`idle`]
//! escalates idle into lock/logout; [`harden`] applies process
//! hardening; [`config`] layers the TOML; [`sdnotify`]/[`metrics`] are
//! supporting cast.

pub mod bus;
pub mod config;
pub mod desktop;
pub mod env;
pub mod harden;
pub mod idle;
pub mod inhibit;
pub mod logind;
pub mod metrics;
pub mod proc;
pub mod sdnotify;
pub mod service;
pub mod session;
pub mod sys;
