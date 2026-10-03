#![forbid(unsafe_code)]
//! Inhibitors: apps block logout/shutdown/suspend/idle/switch-user with a
//! reason (spec 02 §3/§4/§6).
//!
//! Bus contract: `Inhibit(what, who, why) -> fd`. The returned file
//! descriptor owns the inhibitor — closing it releases the inhibitor
//! automatically. That makes leaked inhibitors (client died without
//! releasing) self-healing: the peer end of a socketpair sees EOF exactly
//! when the last fd duplicate in the client process is gone. The watch
//! task in `session.rs` performs the release, so an inhibitor leak costs
//! nothing while the holder lives (no polling, no timers — event-driven).

use crate::lifecycle::InhibitorSnapshot;
use std::collections::BTreeMap;
use std::time::Instant;

/// Valid inhibitor classes (documented in docs/DBUS.md; mirrors logind's
/// set with LionOS naming).
pub const VALID_WHAT: [&str; 5] = ["logout", "shutdown", "suspend", "idle", "switch-user"];

/// Bound inputs (spec 02 §8: "validate and bound every input").
pub const MAX_WHO_LEN: usize = 256;
pub const MAX_WHY_LEN: usize = 512;

/// One held inhibitor.
#[derive(Debug, Clone)]
pub struct Inhibitor {
    pub id: u64,
    pub what: String,
    pub who: String,
    pub why: String,
    /// Bus unique name of the holder (for accounting/diagnostics only —
    /// never used for authorization).
    pub owner: String,
    pub since: Instant,
}

/// Pure store; fd plumbing lives in the bus/session layers.
#[derive(Debug, Default)]
pub struct InhibitorStore {
    next_id: u64,
    active: BTreeMap<u64, Inhibitor>,
}

impl InhibitorStore {
    /// Validate and insert. Returns the inhibitor id.
    pub fn add(
        &mut self,
        what: &str,
        who: &str,
        why: &str,
        owner: &str,
        now: Instant,
        max_active: usize,
    ) -> Result<u64, String> {
        let what = what.trim();
        if !VALID_WHAT.contains(&what) {
            return Err(format!(
                "invalid what {what:?}: expected one of {}",
                VALID_WHAT.join(", ")
            ));
        }
        if who.trim().is_empty() || who.len() > MAX_WHO_LEN {
            return Err(format!("who must be 1..={MAX_WHO_LEN} bytes"));
        }
        if why.len() > MAX_WHY_LEN {
            return Err(format!("why must be at most {MAX_WHY_LEN} bytes"));
        }
        if self.active.len() >= max_active {
            return Err(format!("inhibitor limit reached ({max_active})"));
        }
        self.next_id += 1;
        let id = self.next_id;
        self.active.insert(
            id,
            Inhibitor {
                id,
                what: what.to_string(),
                who: who.trim().to_string(),
                why: why.to_string(),
                owner: owner.to_string(),
                since: now,
            },
        );
        Ok(id)
    }

    /// Release by id (fd EOF path or explicit drop).
    pub fn remove(&mut self, id: u64) -> Option<Inhibitor> {
        self.active.remove(&id)
    }

    /// Current inhibitors as decision snapshots.
    pub fn snapshots(&self) -> Vec<InhibitorSnapshot> {
        self.active
            .values()
            .map(|i| InhibitorSnapshot {
                what: i.what.clone(),
                who: i.who.clone(),
                why: i.why.clone(),
            })
            .collect()
    }

    /// `InhibitedActions` property: the union of active classes, sorted,
    /// unique (D-Bus `as`).
    pub fn inhibited_actions(&self) -> Vec<String> {
        let mut v: Vec<String> = self.active.values().map(|i| i.what.clone()).collect();
        v.sort();
        v.dedup();
        v
    }

    /// Is there any inhibitor of this class?
    pub fn has(&self, what: &str) -> bool {
        self.active.values().any(|i| i.what == what)
    }

    pub fn len(&self) -> usize {
        self.active.len()
    }

    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }

    /// Inhibitors held by a bus client that vanished (defensive: the fd
    /// EOF path normally cleans these up first).
    pub fn retain_owner_gone(&mut self, owner: &str) -> Vec<Inhibitor> {
        let gone: Vec<u64> = self
            .active
            .values()
            .filter(|i| i.owner == owner)
            .map(|i| i.id)
            .collect();
        gone.into_iter().filter_map(|id| self.remove(id)).collect()
    }
}

/// Create the fd pair backing an inhibitor: the *peer* end goes to the
/// D-Bus caller (returned as the method reply's fd); our end stays for the
/// EOF watch task. Socketpair semantics give EOF on last-close.
pub fn inhibitor_fd_pair(
) -> std::io::Result<(tokio::net::UnixStream, std::os::unix::net::UnixStream)> {
    let (ours, theirs) = tokio::net::UnixStream::pair()?;
    let theirs_std = theirs.into_std()?;
    Ok((ours, theirs_std))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(ms: u64) -> Instant {
        Instant::now() + std::time::Duration::from_millis(ms)
    }

    #[test]
    fn add_and_inhibited_actions() {
        let mut s = InhibitorStore::default();
        s.add("shutdown", "lion-text", "unsaved doc", ":1.5", t(0), 64)
            .unwrap();
        s.add("logout", "lion-media", "encoding", ":1.6", t(1), 64)
            .unwrap();
        assert_eq!(s.inhibited_actions(), vec!["logout", "shutdown"]);
        assert!(s.has("shutdown"));
        assert!(!s.has("idle"));
    }

    #[test]
    fn invalid_what_rejected() {
        let mut s = InhibitorStore::default();
        let e = s.add("reboot", "x", "y", ":1.5", t(0), 64).unwrap_err();
        assert!(e.contains("invalid what"));
    }

    #[test]
    fn bounded_strings() {
        let mut s = InhibitorStore::default();
        let long = "x".repeat(300);
        assert!(s.add("logout", &long, "y", ":1.5", t(0), 64).is_err());
        let why = "y".repeat(600);
        assert!(s.add("logout", "who", &why, ":1.5", t(0), 64).is_err());
        assert!(s.add("logout", "  ", "y", ":1.5", t(0), 64).is_err());
    }

    #[test]
    fn max_active_enforced() {
        let mut s = InhibitorStore::default();
        s.add("logout", "a", "a", ":1.1", t(0), 2).unwrap();
        s.add("logout", "b", "b", ":1.2", t(0), 2).unwrap();
        let e = s.add("logout", "c", "c", ":1.3", t(0), 2).unwrap_err();
        assert!(e.contains("limit"));
    }

    #[test]
    fn remove_releases() {
        let mut s = InhibitorStore::default();
        let id = s.add("shutdown", "app", "why", ":1.7", t(0), 64).unwrap();
        s.remove(id);
        assert!(s.is_empty());
        assert_eq!(s.inhibited_actions(), Vec::<String>::new());
    }

    #[test]
    fn owner_gone_sweep() {
        let mut s = InhibitorStore::default();
        s.add("shutdown", "a", "a", ":1.9", t(0), 64).unwrap();
        s.add("logout", "b", "b", ":1.9", t(0), 64).unwrap();
        s.add("logout", "c", "c", ":1.10", t(0), 64).unwrap();
        let gone = s.retain_owner_gone(":1.9");
        assert_eq!(gone.len(), 2);
        assert_eq!(s.len(), 1);
    }

    #[tokio::test]
    async fn fd_pair_eof_on_peer_close() {
        // The core leak-recovery mechanism: when the client's end closes,
        // reading our end yields EOF (0 bytes).
        use tokio::io::AsyncReadExt;
        let (mut ours, theirs) = inhibitor_fd_pair().unwrap();
        drop(theirs);
        let mut buf = [0u8; 1];
        let n = ours.read(&mut buf).await.unwrap();
        assert_eq!(n, 0, "peer close must surface as EOF");
    }

    #[tokio::test]
    async fn fd_pair_open_while_peer_holds() {
        use tokio::io::AsyncReadExt;
        let (mut ours, theirs) = inhibitor_fd_pair().unwrap();
        // Peer still open: read should not observe EOF — test by writing
        // one byte from the peer instead (readable, then EOF later).
        use tokio::io::AsyncWriteExt;
        let mut theirs = tokio::net::UnixStream::from_std(theirs).unwrap();
        theirs.write_all(b"x").await.unwrap();
        let mut buf = [0u8; 1];
        let n = ours.read(&mut buf).await.unwrap();
        assert_eq!(n, 1);
    }
}
