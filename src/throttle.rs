#![forbid(unsafe_code)]
//! Per-caller rate limiting for expensive D-Bus calls (spec 02 §8:
//! "rate-limit expensive calls"). Sliding-window counters keyed by the
//! caller's bus unique name (assigned by the bus daemon — never by
//! caller-supplied strings).
//!
//! Idle means idle: no timers; entries are pruned lazily on access and the
//! key map is bounded so a hostile unique-name spray cannot grow it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Max tracked callers (pruned by staleness; overflow evicts stalest).
const MAX_KEYS: usize = 256;

#[derive(Debug)]
pub struct RateLimiter {
    window: Duration,
    max: u32,
    events: HashMap<String, Vec<Instant>>,
}

impl RateLimiter {
    pub fn new(window: Duration, max: u32) -> RateLimiter {
        RateLimiter {
            window,
            max: max.max(1),
            events: HashMap::new(),
        }
    }

    /// May the caller perform one more call at `now`?
    /// Records the event when allowed.
    pub fn allow(&mut self, key: &str, now: Instant) -> bool {
        let window = self.window;
        let entry = self.events.entry(key.to_string()).or_default();
        entry.retain(|t| now.duration_since(*t) < window);
        if entry.len() >= self.max as usize {
            return false;
        }
        entry.push(now);
        if self.events.len() > MAX_KEYS {
            self.evict_stale(now);
        }
        true
    }

    fn evict_stale(&mut self, now: Instant) {
        let window = self.window;
        self.events.retain(|_, v| {
            v.retain(|t| now.duration_since(*t) < window);
            !v.is_empty()
        });
        // Still too many keys: drop the least-recently-used.
        while self.events.len() > MAX_KEYS {
            if let Some(oldest) = self
                .events
                .iter()
                .min_by_key(|(_, v)| v.last().copied())
                .map(|(k, _)| k.clone())
            {
                self.events.remove(&oldest);
            } else {
                break;
            }
        }
    }

    /// Calls currently counted for a key (diagnostics/tests).
    pub fn count(&self, key: &str) -> usize {
        self.events.get(key).map(|v| v.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(ms: u64) -> Instant {
        Instant::now() + Duration::from_millis(ms)
    }

    #[test]
    fn burst_limit_enforced() {
        let mut r = RateLimiter::new(Duration::from_secs(60), 3);
        assert!(r.allow(":1.5", t(0)));
        assert!(r.allow(":1.5", t(1)));
        assert!(r.allow(":1.5", t(2)));
        assert!(!r.allow(":1.5", t(3)), "4th call within the window denied");
    }

    #[test]
    fn window_slides() {
        let mut r = RateLimiter::new(Duration::from_secs(1), 2);
        assert!(r.allow(":1.5", t(0)));
        assert!(r.allow(":1.5", t(100)));
        assert!(!r.allow(":1.5", t(200)));
        // after the window drained:
        assert!(r.allow(":1.5", t(1200)));
    }

    #[test]
    fn keys_are_independent() {
        let mut r = RateLimiter::new(Duration::from_secs(60), 1);
        assert!(r.allow(":1.5", t(0)));
        assert!(r.allow(":1.6", t(0)));
        assert!(!r.allow(":1.5", t(1)));
        assert!(!r.allow(":1.6", t(1)));
    }

    #[test]
    fn key_spray_bounded() {
        let mut r = RateLimiter::new(Duration::from_secs(60), 1);
        for i in 0..2000 {
            r.allow(&format!(":1.{i}"), t(0));
        }
        assert!(r.events.len() <= MAX_KEYS + 1);
    }

    #[test]
    fn counts_exposed() {
        let mut r = RateLimiter::new(Duration::from_secs(60), 10);
        r.allow(":1.5", t(0));
        r.allow(":1.5", t(1));
        assert_eq!(r.count(":1.5"), 2);
        assert_eq!(r.count(":1.9"), 0);
    }
}
