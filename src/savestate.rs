#![forbid(unsafe_code)]
//! Session save/restore (spec 02 §3, opt-in via `session.restore_apps`):
//! remember open apps and workspaces and relaunch them at next login.
//!
//! v2 milestone: the model + persistence are shipped and tested here; the
//! relaunch path runs through the ordinary autostart-style spawn (the
//! orchestrator in `session.rs` feeds restored records into the launcher
//! once the shell is ready). Apps report themselves via `RegisterClient`
//! plus a state hint — the D-Bus surface for the hint is additive and
//! documented in docs/DBUS.md (see MIGRATION.md).

use serde::{Deserialize, Serialize};
use std::path::Path;

/// One remembered app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppRecord {
    pub app_id: String,
    /// argv to relaunch.
    pub exec: Vec<String>,
    /// Workspace index (0-based).
    #[serde(default)]
    pub workspace: u32,
}

/// Persisted session snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SessionState {
    #[serde(default, rename = "apps")]
    pub apps: Vec<AppRecord>,
    #[serde(default)]
    pub workspaces: u32,
}

/// Bounds: restoring must never fork-bomb the desktop.
pub const MAX_APPS: usize = 64;
pub const MAX_EXEC_LEN: usize = 64;

impl SessionState {
    /// Sanitize after load: bound counts, drop empty/malformed records.
    /// Fail-open (a corrupt record drops, the rest restore) — restore is
    /// a convenience, never a boot dependency.
    pub fn sanitized(self) -> SessionState {
        let mut apps: Vec<AppRecord> = Vec::new();
        for mut a in self.apps {
            if a.app_id.is_empty() || a.app_id.len() > 256 {
                continue;
            }
            if a.exec.is_empty() || a.exec.len() > MAX_EXEC_LEN {
                continue;
            }
            if a.exec
                .iter()
                .any(|s| s.is_empty() || s.len() > 4096 || s.contains('\0'))
            {
                continue;
            }
            if apps.len() >= MAX_APPS {
                break;
            }
            a.workspace = a.workspace.min(31);
            apps.push(a);
        }
        SessionState {
            apps,
            workspaces: self.workspaces.min(16),
        }
    }

    /// Load + sanitize; None when missing (first login) — a corrupt file
    /// logs a warning and restores nothing.
    pub fn load(path: &Path) -> Option<SessionState> {
        let raw = std::fs::read_to_string(path).ok()?;
        match serde_json::from_str::<SessionState>(&raw) {
            Ok(s) => Some(s.sanitized()),
            Err(e) => {
                tracing::warn!(target: "savestate", "session state corrupt, not restoring: {e}");
                None
            }
        }
    }

    /// Persist (best effort, atomic rename).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.new");
        let raw = serde_json::to_vec_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(&tmp, raw)?;
        std::fs::rename(&tmp, path)
    }

    /// Record an app (dedup by app_id; the last workspace wins).
    pub fn upsert_app(&mut self, app_id: &str, exec: Vec<String>, workspace: u32) {
        if let Some(existing) = self.apps.iter_mut().find(|a| a.app_id == app_id) {
            existing.exec = exec;
            existing.workspace = workspace;
        } else if self.apps.len() < MAX_APPS {
            self.apps.push(AppRecord {
                app_id: app_id.to_string(),
                exec,
                workspace,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> SessionState {
        SessionState {
            apps: vec![
                AppRecord {
                    app_id: "lion-text".into(),
                    exec: vec!["lion-text".into(), "--file".into(), "a.txt".into()],
                    workspace: 1,
                },
                AppRecord {
                    app_id: "lion-terminal".into(),
                    exec: vec!["lion-terminal".into()],
                    workspace: 0,
                },
            ],
            workspaces: 4,
        }
    }

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.json");
        state().save(&p).unwrap();
        assert_eq!(SessionState::load(&p), Some(state()));
    }

    #[test]
    fn missing_is_none() {
        assert!(SessionState::load(Path::new("/nonexistent/s.json")).is_none());
    }

    #[test]
    fn corrupt_is_none_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.json");
        std::fs::write(&p, "]]not json[[[").unwrap();
        assert!(SessionState::load(&p).is_none());
    }

    #[test]
    fn sanitize_bounds() {
        let s = SessionState {
            apps: (0..100)
                .map(|i| AppRecord {
                    app_id: format!("app{i}"),
                    exec: vec!["x".into()],
                    workspace: 99,
                })
                .collect(),
            workspaces: 99,
        };
        let clean = s.sanitized();
        assert_eq!(clean.apps.len(), MAX_APPS);
        assert_eq!(clean.workspaces, 16);
        assert!(clean.apps.iter().all(|a| a.workspace <= 31));
    }

    #[test]
    fn sanitize_drops_bad_records() {
        let s = SessionState {
            apps: vec![
                AppRecord {
                    app_id: String::new(),
                    exec: vec!["x".into()],
                    workspace: 0,
                },
                AppRecord {
                    app_id: "ok".into(),
                    exec: vec![],
                    workspace: 0,
                },
                AppRecord {
                    app_id: "good".into(),
                    exec: vec!["good-bin".into()],
                    workspace: 2,
                },
            ],
            workspaces: 1,
        };
        let clean = s.sanitized();
        assert_eq!(clean.apps.len(), 1);
        assert_eq!(clean.apps[0].app_id, "good");
    }

    #[test]
    fn upsert_dedups() {
        let mut s = SessionState::default();
        s.upsert_app("a", vec!["a".into()], 0);
        s.upsert_app("a", vec!["a2".into()], 3);
        assert_eq!(s.apps.len(), 1);
        assert_eq!(s.apps[0].exec, vec!["a2".to_string()]);
        assert_eq!(s.apps[0].workspace, 3);
    }
}
