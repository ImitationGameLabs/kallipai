//! Minimal per-IP token bucket, used to guard the unauthenticated,
//! crypto-expensive `/v1/auth/*` begin endpoints against invite enumeration
//! and ceremony-spam.
//!
//! One bucket per client IP, lazily created on first sighting. Buckets are
//! never actively evicted (the map is bounded in practice by the distinct-IP
//! cardinality of the auth surface, which is tiny for an invite-only deploy);
//! a future hardening pass can add an LRU sweep. All math is synchronous and
//! cheap, so a `std::sync::Mutex` is appropriate.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant};

/// A fractional-capacity token bucket (the classic GCRA / leaky-bucket reading:
/// continuous refill, hard capacity cap).
struct Bucket {
    tokens: f64,
    last: Instant,
}

/// Per-IP rate limiter. Lives in `AppState` (inside the `Arc` of
/// `SharedState`) and is shared by reference from the auth-rate-limit
/// middleware; constructed once at boot and never cloned.
pub struct IpRateLimiter {
    inner: Mutex<HashMap<IpAddr, Bucket>>,
    capacity: u32,
    refill_per_sec: u32,
}

impl IpRateLimiter {
    /// New limiter: `capacity` burst, refilling at `refill_per_sec` tokens/sec.
    pub fn new(capacity: u32, refill_per_sec: u32) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            capacity,
            refill_per_sec,
        }
    }

    /// Consume one token for `ip`. Returns `true` if allowed (a token was
    /// available), `false` if the bucket is empty (caller surfaces 429). On
    /// mutex poisoning (a prior panic under the lock — process-terminal) the
    /// call fails closed (`false`) and warns once.
    pub fn check(&self, ip: IpAddr) -> bool {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => {
                static ONCE: Once = Once::new();
                ONCE.call_once(|| {
                    tracing::warn!(
                        "rate-limiter mutex poisoned; denying all auth requests until restart"
                    )
                });
                return false;
            }
        };
        let now = Instant::now();
        let bucket = guard.entry(ip).or_insert_with(|| Bucket {
            // A first-time caller starts with a full bucket.
            tokens: self.capacity as f64,
            last: now,
        });
        let elapsed = now.duration_since(bucket.last);
        bucket.last = now;
        bucket.tokens += tokens_refilled(elapsed, self.refill_per_sec);
        if bucket.tokens > self.capacity as f64 {
            bucket.tokens = self.capacity as f64;
        }
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Tokens accrued over `elapsed` at `refill_per_sec`.
fn tokens_refilled(elapsed: Duration, refill_per_sec: u32) -> f64 {
    elapsed.as_secs_f64() * refill_per_sec as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn first_caller_starts_with_capacity() {
        let rl = IpRateLimiter::new(3, 1);
        let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        assert!(rl.check(ip));
        assert!(rl.check(ip));
        assert!(rl.check(ip));
        // 4th within the same instant exhausts the bucket.
        assert!(!rl.check(ip));
    }

    #[test]
    fn distinct_ips_are_independent() {
        let rl = IpRateLimiter::new(1, 1);
        let a = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        let b = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2));
        assert!(rl.check(a));
        assert!(rl.check(b));
        // `a` is exhausted; `b` is not.
        assert!(!rl.check(a));
    }
}
