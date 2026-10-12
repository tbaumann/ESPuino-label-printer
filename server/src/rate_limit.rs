//! A small, in-memory, fixed-window rate limiter.
//!
//! Used to stop a caller from hammering Stripe by opening checkout sessions in
//! a loop. It lives in process memory on purpose: the window is short, the
//! state is tiny, and a restart simply forgets who was being throttled — the
//! same trade-off any single-binary service makes for this kind of defence.
//!
//! The map is bounded by the number of distinct keys seen inside one window:
//! anything whose window has fully elapsed is dropped lazily on the next call.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A keyed fixed-window counter: at most `limit` calls are admitted per
/// `window` for each key, independently. Cheap to clone (a shared map behind
/// an `Arc`).
#[derive(Clone)]
pub struct RateLimiter {
    limit: u32,
    window_secs: u64,
    buckets: Arc<Mutex<HashMap<String, Bucket>>>,
}

#[derive(Clone, Copy)]
struct Bucket {
    window_start: u64,
    count: u32,
}

impl RateLimiter {
    pub fn new(limit: u32, window: Duration) -> Self {
        assert!(limit > 0, "a rate limit of zero would block everything");
        assert!(!window.is_zero(), "a zero-width window is nonsensical");
        Self {
            limit,
            window_secs: window.as_secs().max(1),
            buckets: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Try to admit one call for `key`. Returns `false` when the key already
    /// spent its whole allowance in the current window.
    pub fn allow(&self, key: &str) -> bool {
        self.allow_at(key, unix_now())
    }

    /// The decision, with the clock injected, so tests can step time instead
    /// of sleeping.
    fn allow_at(&self, key: &str, now: u64) -> bool {
        let mut buckets = self.buckets.lock().unwrap();
        let window_start = now / self.window_secs * self.window_secs;

        // Lazy cleanup: drop any bucket whose window has already fully
        // elapsed. Bounded by the keys seen within one window, so an attacker
        // spraying keys cannot grow the map without bound.
        buckets.retain(|_, b| b.window_start + self.window_secs > now);

        let b = match buckets.get_mut(key) {
            Some(b) => b,
            None => {
                buckets.insert(
                    key.to_string(),
                    Bucket {
                        window_start,
                        count: 0,
                    },
                );
                buckets.get_mut(key).unwrap()
            }
        };
        if b.window_start < window_start {
            *b = Bucket {
                window_start,
                count: 0,
            };
        }
        if b.count >= self.limit {
            return false;
        }
        b.count += 1;
        true
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limiter() -> RateLimiter {
        RateLimiter::new(2, Duration::from_secs(60))
    }

    #[test]
    fn admits_up_to_the_limit_then_rejects() {
        let l = limiter();
        // All within the same 60..119 window.
        assert!(l.allow_at("p", 100));
        assert!(l.allow_at("p", 110));
        assert!(!l.allow_at("p", 115)); // limit 2 reached, still in window
    }

    #[test]
    fn window_resets_on_the_boundary() {
        let l = limiter();
        assert!(l.allow_at("p", 0)); // window 0..59
        assert!(l.allow_at("p", 59));
        assert!(!l.allow_at("p", 59));
        assert!(l.allow_at("p", 60)); // window 60..119 starts fresh
    }

    #[test]
    fn keys_are_independent() {
        let l = limiter();
        assert!(l.allow_at("a", 100));
        assert!(l.allow_at("b", 100));
        assert!(l.allow_at("a", 100)); // second for a; b still has its own
        assert!(!l.allow_at("a", 100));
        assert!(l.allow_at("b", 100));
    }

    #[test]
    fn stale_buckets_are_forgotten() {
        // A key denied at the start of a window is allowed again once that
        // window has elapsed — the bucket was pruned, not kept forever.
        let l = limiter();
        assert!(l.allow_at("a", 0)); // window 0..59, a is at its limit
        assert!(l.allow_at("a", 0));
        assert!(!l.allow_at("a", 0));
        assert!(l.allow_at("b", 5 * 60)); // prunes a; window starts fresh
        assert!(l.allow_at("a", 5 * 60)); // a's allowance is back
    }
}
