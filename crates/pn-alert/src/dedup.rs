//! Cooldown-based alert deduplication.
//!
//! [`AlertDedup`] tracks the last-fired timestamp for every alert in a
//! [`DashMap`] so that multiple price ticks within the cooldown window do not
//! produce duplicate notifications.  The map is lock-free for concurrent
//! readers and writers, making it safe to share across tasks.

use chrono::{Duration, NaiveDateTime, Utc};
use dashmap::DashMap;

use crate::rules::AlertRule;

/// In-memory store that tracks when each alert last fired.
///
/// Cheap to clone because [`DashMap`] is reference-counted internally.
///
/// # Example
///
/// ```
/// use pn_alert::dedup::AlertDedup;
///
/// let dedup = AlertDedup::new();
///
/// // First call always allowed
/// assert!(dedup.can_fire(42, 60));
///
/// // After marking fired, the alert is in cooldown
/// dedup.mark_fired(42);
/// assert!(!dedup.can_fire(42, 60));
/// ```
pub struct AlertDedup {
    /// Maps `alert_id` to the UTC timestamp of the most recent firing.
    last_triggered: DashMap<i64, NaiveDateTime>,
}

impl AlertDedup {
    /// Creates an empty deduplication store.
    pub fn new() -> Self {
        Self {
            last_triggered: DashMap::new(),
        }
    }

    /// Returns `true` when the alert is not currently in its cooldown window
    /// and may fire again.
    ///
    /// An alert with no recorded last-fire time is always allowed to fire.
    ///
    /// # Arguments
    ///
    /// * `alert_id` – The primary key of the alert row.
    /// * `cooldown_minutes` – How many minutes must elapse after the previous
    ///   firing before the alert may fire again.
    pub fn can_fire(&self, alert_id: i64, cooldown_minutes: i32) -> bool {
        if let Some(last) = self.last_triggered.get(&alert_id) {
            let cooldown = Duration::minutes(cooldown_minutes as i64);
            Utc::now().naive_utc() - *last > cooldown
        } else {
            true
        }
    }

    /// Records the current UTC time as the last-fire timestamp for `alert_id`.
    ///
    /// Call this immediately after a notification has been enqueued so that
    /// subsequent price ticks within the cooldown window are suppressed.
    pub fn mark_fired(&self, alert_id: i64) {
        self.last_triggered.insert(alert_id, Utc::now().naive_utc());
    }

    /// Bulk-loads initial state from the database rows fetched at startup.
    ///
    /// Alerts that have a recorded `last_triggered_at` are inserted into the
    /// map, so the engine respects cooldowns that survived a process restart.
    ///
    /// # Arguments
    ///
    /// * `alerts` – Slice of all active [`AlertRule`]s returned by the initial
    ///   cache refresh.
    pub fn load_from_alerts(&self, alerts: &[AlertRule]) {
        for alert in alerts {
            if let Some(last) = alert.last_triggered_at {
                self.last_triggered.insert(alert.alert_id, last);
            }
        }
    }
}

impl Default for AlertDedup {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_alert_can_always_fire() {
        let dedup = AlertDedup::new();
        assert!(dedup.can_fire(1, 60));
    }

    #[test]
    fn after_mark_fired_alert_is_in_cooldown() {
        let dedup = AlertDedup::new();
        dedup.mark_fired(1);
        // With a 60-minute cooldown the alert cannot fire immediately after
        assert!(!dedup.can_fire(1, 60));
    }

    #[test]
    fn zero_cooldown_always_allows_fire() {
        let dedup = AlertDedup::new();
        dedup.mark_fired(1);
        // 0-minute cooldown means the elapsed time is always > 0 minutes
        assert!(dedup.can_fire(1, 0));
    }

    #[test]
    fn load_from_alerts_populates_past_triggers() {
        use crate::rules::AlertRule;
        use pn_common::models::AlertType;
        use rust_decimal::dec;

        let past = Utc::now().naive_utc() - Duration::minutes(30);
        let rule = AlertRule {
            alert_id: 7,
            subscription_id: 1,
            user_telegram_id: 1,
            bot_id: "bot".to_string(),
            token_id: "tok".to_string(),
            outcome_index: 0,
            market_question: "Q?".to_string(),
            alert_type: AlertType::Above,
            threshold: dec!(0.5),
            cooldown_minutes: 60,
            last_triggered_at: Some(past),
        };

        let dedup = AlertDedup::new();
        dedup.load_from_alerts(&[rule]);

        // 30 minutes elapsed, 60-minute cooldown → still in cooldown
        assert!(!dedup.can_fire(7, 60));
        // 30-minute cooldown → cooldown has expired
        assert!(dedup.can_fire(7, 29));
    }
}
