#![forbid(unsafe_code)]
//! Safe mode (spec 02 §3): if the previous start crashed repeatedly, boot
//! a minimal shell (compositor + terminal + settings) and tell the user
//! why.
//!
//! A "bad start" = the compositor died before the session reached
//! `SessionReady`. The counter persists in
//! `$XDG_STATE_HOME/lion-session/history.json`; a clean shutdown resets
//! it. The decision itself is pure so tests never touch the clock.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Persistent session history.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct History {
    /// Consecutive starts that ended in a pre-ready compositor crash.
    #[serde(default)]
    pub consecutive_bad_starts: u32,
    /// How the last session ended: "clean" | "crash" | "unknown".
    #[serde(default)]
    pub last_end: String,
}

/// The safe-mode decision for this boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafeModeDecision {
    /// Boot the minimal shell instead of the full plan?
    pub active: bool,
    /// Human-facing reason (logged + surfaced via the SafeMode property).
    pub reason: String,
}

/// Pure decision: `consecutive_bad_starts >= threshold` → safe mode.
pub fn decide(history: &History, threshold: u32) -> SafeModeDecision {
    if history.consecutive_bad_starts >= threshold {
        SafeModeDecision {
            active: true,
            reason: format!(
                "the previous {} start(s) ended in a compositor crash; booting the minimal shell",
                history.consecutive_bad_starts
            ),
        }
    } else {
        SafeModeDecision {
            active: false,
            reason: String::new(),
        }
    }
}

impl History {
    /// Load; a missing file is a clean slate (first boot), a corrupt file
    /// is a clean slate with a loud log (safe mode must never brick boot,
    /// and must not linger either — spec 02 §6).
    pub fn load(path: &Path) -> History {
        match std::fs::read_to_string(path) {
            Ok(raw) => match serde_json::from_str(&raw) {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!(target: "safemode", "history corrupt, resetting: {e}");
                    History::default()
                }
            },
            Err(_) => History::default(),
        }
    }

    /// Persist (best effort; state dir may be read-only in odd setups).
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Io("state dir".into(), e))?;
        }
        let raw = serde_json::to_vec_pretty(self)
            .map_err(|e| Error::Other(format!("serialize history: {e}")))?;
        // Atomic-ish: write alongside, then rename.
        let tmp = path.with_extension("json.new");
        std::fs::write(&tmp, &raw).map_err(|e| Error::Io("history write".into(), e))?;
        std::fs::rename(&tmp, path).map_err(|e| Error::Io("history rename".into(), e))?;
        Ok(())
    }

    /// The compositor died before the session was ready.
    pub fn record_bad_start(&mut self) {
        self.consecutive_bad_starts = self.consecutive_bad_starts.saturating_add(1);
        self.last_end = "crash".into();
    }

    /// The session reached ready (resets the counter) — call once on
    /// SessionReady.
    pub fn record_ready(&mut self) {
        // Bad-start counting is per boot attempt: reaching ready means the
        // attempt was good even if it ends later.
        self.last_end = "unknown".into();
    }

    /// The session ended by user request / clean teardown.
    pub fn record_clean_end(&mut self) {
        self.consecutive_bad_starts = 0;
        self.last_end = "clean".into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_boot_is_not_safe_mode() {
        assert!(!decide(&History::default(), 2).active);
    }

    #[test]
    fn threshold_trips() {
        let mut h = History::default();
        h.record_bad_start();
        h.record_bad_start();
        let d = decide(&h, 2);
        assert!(d.active);
        assert!(d.reason.contains("minimal shell"));
        assert!(d.reason.contains("2"));
    }

    #[test]
    fn clean_end_resets() {
        let mut h = History::default();
        h.record_bad_start();
        h.record_bad_start();
        h.record_clean_end();
        assert!(!decide(&h, 2).active);
        assert_eq!(h.last_end, "clean");
    }

    #[test]
    fn ready_does_not_reset_bad_starts() {
        // Only a clean END resets: crash-after-ready counts toward the
        // *next* decision? No — spec: "previous start crashed repeatedly".
        // A start that reached ready then crashed is still a bad start
        // only if it died before ready; after ready, the compositor death
        // ends the session but the start itself was successful.
        let mut h = History::default();
        h.record_bad_start();
        h.record_ready();
        assert_eq!(h.consecutive_bad_starts, 1, "ready alone does not reset");
        h.record_clean_end();
        assert_eq!(h.consecutive_bad_starts, 0);
    }

    #[test]
    fn history_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("history.json");
        let mut h = History::default();
        h.record_bad_start();
        h.save(&p).unwrap();
        let h2 = History::load(&p);
        assert_eq!(h2.consecutive_bad_starts, 1);
        assert_eq!(h2.last_end, "crash");
    }

    #[test]
    fn corrupt_history_resets() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("history.json");
        std::fs::write(&p, "{not json").unwrap();
        assert_eq!(History::load(&p).consecutive_bad_starts, 0);
    }

    #[test]
    fn missing_history_is_first_boot() {
        assert_eq!(
            History::load(Path::new("/nonexistent/h.json")).consecutive_bad_starts,
            0
        );
    }
}
