//! Inter-component channel event types.
//!
//! These types flow through `tokio::sync::broadcast` or `mpsc` channels
//! between the monitor, alert engine, and notification dispatcher.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// PriceUpdate
// ---------------------------------------------------------------------------

/// A real-time price tick received from the Polymarket WebSocket feed.
///
/// Broadcast by the monitor crate; consumed by the alert engine.
///
/// # Example
///
/// ```
/// use chrono::Utc;
/// use rust_decimal_macros::dec;
/// use pn_common::events::PriceUpdate;
///
/// let tick = PriceUpdate {
///     token_id: "0xabc".to_string(),
///     condition_id: "0xdef".to_string(),
///     price: dec!(0.72),
///     timestamp: Utc::now(),
/// };
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceUpdate {
    /// The CLOB token ID whose price changed.
    pub token_id: String,
    /// The parent condition (market) this token belongs to.
    pub condition_id: String,
    /// New best-price (mid-price or last trade) expressed as a decimal in
    /// the range `[0, 1]`.
    pub price: Decimal,
    /// Wall-clock time at which the update was received.
    pub timestamp: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// NotificationRequest
// ---------------------------------------------------------------------------

/// Which kind of notification should be sent to the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotificationType {
    /// A triggered price alert.
    Alert,
    /// The scheduled daily portfolio summary.
    DailySummary,
}

impl std::fmt::Display for NotificationType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotificationType::Alert => write!(f, "alert"),
            NotificationType::DailySummary => write!(f, "daily_summary"),
        }
    }
}

/// A request to deliver a Telegram message to a specific user.
///
/// Produced by the alert engine and scheduler; consumed by the notification
/// dispatcher (`pn-notify`).
///
/// # Example
///
/// ```
/// use pn_common::events::{NotificationRequest, NotificationType};
///
/// let req = NotificationRequest {
///     user_telegram_id: 123456789,
///     bot_id: "main_bot".to_string(),
///     message: "YES on 'Will X happen?' crossed 70¢".to_string(),
///     notification_type: NotificationType::Alert,
/// };
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationRequest {
    /// Recipient's Telegram numeric user ID.
    pub user_telegram_id: i64,
    /// Identifies which bot should send this message (for multi-bot setups).
    pub bot_id: String,
    /// Fully-formatted message text (Markdown or plain text).
    pub message: String,
    /// Category used for logging and rate-limiting decisions.
    pub notification_type: NotificationType,
}
