//! Per-user sliding-window rate limiter.
//!
//! Uses a fixed 60-second window tracked in a [`DashMap`] so it is safe to
//! share across async tasks without an explicit `Mutex`.

use chrono::{NaiveDateTime, Utc};
use dashmap::DashMap;

/// Tracks how many messages each Telegram user has received in the current
/// 60-second window and rejects requests that would exceed the cap.
///
/// The map stores `(count, window_start)` per `user_telegram_id`.  When more
/// than 60 seconds have elapsed since `window_start` the counter resets.
///
/// # Example
///
/// ```
/// use pn_notify::rate_limiter::RateLimiter;
///
/// let rl = RateLimiter::new(5);
/// // First five calls within a minute are allowed.
/// for _ in 0..5 {
///     assert!(rl.check_and_consume(123456));
/// }
/// // The sixth is rejected.
/// assert!(!rl.check_and_consume(123456));
/// ```
pub struct RateLimiter {
    /// `user_telegram_id` → `(messages_sent_in_window, window_start_utc)`
    windows: DashMap<i64, (u32, NaiveDateTime)>,
    /// Maximum messages allowed per 60-second window per user.
    max_per_minute: u32,
}

impl RateLimiter {
    /// Create a new rate limiter with the given per-user cap.
    pub fn new(max_per_minute: u32) -> Self {
        Self {
            windows: DashMap::new(),
            max_per_minute,
        }
    }

    /// Returns `true` if the user is within their rate limit and increments the
    /// counter, or `false` if the limit has already been reached for this
    /// 60-second window.
    ///
    /// The window resets automatically once 60 seconds have passed since
    /// `window_start`.
    pub fn check_and_consume(&self, user_id: i64) -> bool {
        let now = Utc::now().naive_utc();
        let mut entry = self.windows.entry(user_id).or_insert((0, now));
        let (count, window_start) = entry.value_mut();

        // Reset the counter when the current window has expired.
        if (now - *window_start).num_seconds() >= 60 {
            *count = 0;
            *window_start = now;
        }

        if *count >= self.max_per_minute {
            false
        } else {
            *count += 1;
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_max_per_minute() {
        let rl = RateLimiter::new(3);
        assert!(rl.check_and_consume(1));
        assert!(rl.check_and_consume(1));
        assert!(rl.check_and_consume(1));
        assert!(!rl.check_and_consume(1));
    }

    #[test]
    fn independent_users_have_independent_windows() {
        let rl = RateLimiter::new(1);
        assert!(rl.check_and_consume(1));
        assert!(!rl.check_and_consume(1));
        // User 2 has not consumed any quota yet.
        assert!(rl.check_and_consume(2));
    }

    #[test]
    fn zero_limit_always_rejects() {
        let rl = RateLimiter::new(0);
        assert!(!rl.check_and_consume(99));
    }
}
