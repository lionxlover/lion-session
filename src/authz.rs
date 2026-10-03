#![forbid(unsafe_code)]
//! Authorization policy (spec 02 §8).
//!
//! The authoritative path is `lion-auth` (os.lionos.Auth1) — wired in
//! `backends.rs` as [`crate::ports::Authorizer`] and fail-closed on
//! transport errors. This module provides the *local policy* used when
//! lion-auth is explicitly disabled (empty bus name — dev/tests) and the
//! action ids every authorizer agrees on.
//!
//! Callers are identified by the D-Bus daemon (bus-verified uid); the
//! session owner may always act on their own session. Power actions on
//! the local fallback additionally require uid 0, the session owner, or
//! an explicit `power_allowed_uids` entry.

use crate::config::AuthConfig;
use std::collections::HashSet;

/// Stable action ids.
pub mod actions {
    pub const LOGOUT: &str = "session.logout";
    pub const RESTART: &str = "power.restart";
    pub const SHUTDOWN: &str = "power.shutdown";
    pub const SUSPEND: &str = "power.suspend";
    pub const HIBERNATE: &str = "power.hibernate";
    pub const LOCK: &str = "session.lock";
    pub const SWITCH_USER: &str = "session.switch-user";
    pub const INHIBIT: &str = "session.inhibit";
    pub const REGISTER: &str = "session.register-client";
    /// Read-only diagnostics (GetBlockers, properties) — not gated.
    pub const READ: &str = "session.read";
}

/// Local-policy fallback.
#[derive(Debug, Clone)]
pub struct LocalPolicy {
    owner_uid: u32,
    session_allowed: HashSet<u32>,
    power_allowed: HashSet<u32>,
}

impl LocalPolicy {
    pub fn new(owner_uid: u32, auth_cfg: &AuthConfig, power_uids: &[u32]) -> LocalPolicy {
        LocalPolicy {
            owner_uid,
            session_allowed: auth_cfg.allowed_uids.iter().copied().collect(),
            power_allowed: power_uids.iter().copied().collect(),
        }
    }

    /// The uid the session belongs to (typically from $UID / logind).
    pub fn owner_uid(&self) -> u32 {
        self.owner_uid
    }

    /// Decide. Session-scoped actions: owner, allowlist, root. Power
    /// actions: owner (desktop standard — the active local user may
    /// suspend/power off their machine), explicit power allowlist, root.
    pub fn authorize(&self, action: &str, uid: u32) -> bool {
        if uid == 0 {
            return true;
        }
        let is_power = action.starts_with("power.");
        if uid == self.owner_uid {
            return true;
        }
        if is_power {
            self.power_allowed.contains(&uid)
        } else {
            self.session_allowed.contains(&uid)
        }
    }
}

/// The effective uid of this process (session owner when running as the
/// user's session daemon).
pub fn current_uid() -> u32 {
    // SAFETY-free: /proc/self/status is world-readable and stable.
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("Uid:") {
                if let Some(first) = rest.split_whitespace().next() {
                    if let Ok(uid) = first.parse::<u32>() {
                        return uid;
                    }
                }
            }
        }
    }
    // Fallback: sysffi (audited) — getuid is the unsafe libc call.
    crate::sysffi::getuid()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AuthConfig {
        AuthConfig {
            bus_name: String::new(),
            timeout_ms: 500,
            allowed_uids: vec![980],
        }
    }

    #[test]
    fn owner_can_do_everything() {
        let p = LocalPolicy::new(1000, &cfg(), &[]);
        for a in [
            actions::LOGOUT,
            actions::SHUTDOWN,
            actions::SUSPEND,
            actions::INHIBIT,
        ] {
            assert!(p.authorize(a, 1000), "{a} denied for owner");
        }
    }

    #[test]
    fn stranger_denied_everything() {
        let p = LocalPolicy::new(1000, &cfg(), &[]);
        assert!(!p.authorize(actions::LOGOUT, 1001));
        assert!(!p.authorize(actions::SHUTDOWN, 1001));
    }

    #[test]
    fn session_allowlist_scopes_session_actions() {
        let p = LocalPolicy::new(1000, &cfg(), &[]);
        assert!(p.authorize(actions::REGISTER, 980), "allowlisted uid");
        assert!(!p.authorize(actions::SHUTDOWN, 980), "session list ≠ power");
    }

    #[test]
    fn power_allowlist_grants_power_only() {
        let p = LocalPolicy::new(1000, &cfg(), &[1001]);
        assert!(p.authorize(actions::SHUTDOWN, 1001));
        assert!(!p.authorize(actions::LOGOUT, 1001));
    }

    #[test]
    fn root_always_allowed() {
        let p = LocalPolicy::new(1000, &cfg(), &[]);
        assert!(p.authorize(actions::SHUTDOWN, 0));
        assert!(p.authorize(actions::LOGOUT, 0));
    }

    #[test]
    fn current_uid_matches_env() {
        // Runs as the sandbox user; getuid via libc equals our /proc parse.
        let via_proc = {
            let status = std::fs::read_to_string("/proc/self/status").unwrap();
            status
                .lines()
                .find(|l| l.starts_with("Uid:"))
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap()
                .parse::<u32>()
                .unwrap()
        };
        assert_eq!(current_uid(), via_proc);
    }
}
