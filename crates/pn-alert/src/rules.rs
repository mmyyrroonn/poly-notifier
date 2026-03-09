//! Alert rule evaluation logic.
//!
//! An [`AlertRule`] is the denormalised, in-memory view of an alert joined
//! with its parent subscription, market, and user.  The [`AlertRule::evaluate`]
//! method is pure and allocation-free – it only inspects prices.

use chrono::NaiveDateTime;
use pn_common::models::AlertType;
use rust_decimal::Decimal;

/// Denormalised in-memory representation of an active alert, ready for
/// evaluation against a price tick.
///
/// All fields are populated once during cache refresh by joining the
/// `alerts`, `subscriptions`, `markets`, and `users` tables.
///
/// # Example
///
/// ```
/// use pn_alert::rules::{AlertRule};
/// use pn_common::models::AlertType;
/// use rust_decimal::dec;
///
/// let rule = AlertRule {
///     alert_id: 1,
///     subscription_id: 10,
///     user_telegram_id: 999,
///     bot_id: "main".to_string(),
///     token_id: "0xabc".to_string(),
///     outcome_index: 0,
///     market_question: "Will X happen?".to_string(),
///     alert_type: AlertType::Above,
///     threshold: dec!(0.70),
///     cooldown_minutes: 60,
///     last_triggered_at: None,
/// };
///
/// assert!(rule.evaluate(dec!(0.75), None));
/// assert!(!rule.evaluate(dec!(0.50), None));
/// ```
#[derive(Debug, Clone)]
pub struct AlertRule {
    pub alert_id: i64,
    pub subscription_id: i64,
    pub user_telegram_id: i64,
    pub bot_id: String,
    pub token_id: String,
    pub outcome_index: i32,
    pub market_question: String,
    pub alert_type: AlertType,
    pub threshold: Decimal,
    pub cooldown_minutes: i32,
    pub last_triggered_at: Option<NaiveDateTime>,
}

impl AlertRule {
    /// Returns `true` when the alert should fire given the current price and
    /// an optional previous price (required for [`AlertType::Cross`]).
    ///
    /// # Cross semantics
    ///
    /// A cross fires when the price transitions from one side of `threshold`
    /// to the other.  If `previous_price` is `None`, `Cross` always returns
    /// `false` because there is nothing to compare against.
    ///
    /// # Examples
    ///
    /// ```
    /// use pn_alert::rules::AlertRule;
    /// use pn_common::models::AlertType;
    /// use rust_decimal::dec;
    ///
    /// let mut rule = AlertRule {
    ///     alert_id: 1,
    ///     subscription_id: 1,
    ///     user_telegram_id: 1,
    ///     bot_id: "bot".to_string(),
    ///     token_id: "tok".to_string(),
    ///     outcome_index: 0,
    ///     market_question: "Q".to_string(),
    ///     alert_type: AlertType::Cross,
    ///     threshold: dec!(0.50),
    ///     cooldown_minutes: 60,
    ///     last_triggered_at: None,
    /// };
    ///
    /// // Upward cross
    /// assert!(rule.evaluate(dec!(0.55), Some(dec!(0.45))));
    /// // Downward cross
    /// assert!(rule.evaluate(dec!(0.45), Some(dec!(0.55))));
    /// // No previous price – cannot detect cross
    /// assert!(!rule.evaluate(dec!(0.55), None));
    /// ```
    pub fn evaluate(&self, current_price: Decimal, previous_price: Option<Decimal>) -> bool {
        match self.alert_type {
            AlertType::Above => current_price >= self.threshold,
            AlertType::Below => current_price <= self.threshold,
            AlertType::Cross => {
                if let Some(prev) = previous_price {
                    // Upward cross: was strictly below, now at or above
                    let upward = prev < self.threshold && current_price >= self.threshold;
                    // Downward cross: was strictly above, now at or below
                    let downward = prev > self.threshold && current_price <= self.threshold;
                    upward || downward
                } else {
                    false
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pn_common::models::AlertType;
    use rust_decimal::dec;

    fn make_rule(alert_type: AlertType, threshold: Decimal) -> AlertRule {
        AlertRule {
            alert_id: 1,
            subscription_id: 1,
            user_telegram_id: 1,
            bot_id: "bot".to_string(),
            token_id: "tok".to_string(),
            outcome_index: 0,
            market_question: "Test?".to_string(),
            alert_type,
            threshold,
            cooldown_minutes: 60,
            last_triggered_at: None,
        }
    }

    #[test]
    fn above_fires_at_and_above_threshold() {
        let rule = make_rule(AlertType::Above, dec!(0.70));
        assert!(rule.evaluate(dec!(0.70), None));
        assert!(rule.evaluate(dec!(0.75), None));
        assert!(!rule.evaluate(dec!(0.69), None));
    }

    #[test]
    fn below_fires_at_and_below_threshold() {
        let rule = make_rule(AlertType::Below, dec!(0.30));
        assert!(rule.evaluate(dec!(0.30), None));
        assert!(rule.evaluate(dec!(0.20), None));
        assert!(!rule.evaluate(dec!(0.31), None));
    }

    #[test]
    fn cross_upward() {
        let rule = make_rule(AlertType::Cross, dec!(0.50));
        assert!(rule.evaluate(dec!(0.55), Some(dec!(0.45))));
    }

    #[test]
    fn cross_downward() {
        let rule = make_rule(AlertType::Cross, dec!(0.50));
        assert!(rule.evaluate(dec!(0.45), Some(dec!(0.55))));
    }

    #[test]
    fn cross_no_previous_price_returns_false() {
        let rule = make_rule(AlertType::Cross, dec!(0.50));
        assert!(!rule.evaluate(dec!(0.55), None));
    }

    #[test]
    fn cross_does_not_fire_when_same_side() {
        let rule = make_rule(AlertType::Cross, dec!(0.50));
        // Both above – no cross
        assert!(!rule.evaluate(dec!(0.60), Some(dec!(0.55))));
        // Both below – no cross
        assert!(!rule.evaluate(dec!(0.40), Some(dec!(0.45))));
    }
}
