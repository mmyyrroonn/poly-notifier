//! Database model types that map 1-to-1 onto the SQLite schema.
//!
//! All types derive [`sqlx::FromRow`] so they can be returned directly from
//! `sqlx::query_as!` / `query_as` calls.  [`serde`] derives allow them to be
//! serialized for the admin API or event payloads.

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

// ---------------------------------------------------------------------------
// users
// ---------------------------------------------------------------------------

/// Subscription tier for a user account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
pub enum UserTier {
    Free,
    Premium,
    Unlimited,
}

impl std::fmt::Display for UserTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UserTier::Free => write!(f, "free"),
            UserTier::Premium => write!(f, "premium"),
            UserTier::Unlimited => write!(f, "unlimited"),
        }
    }
}

impl std::str::FromStr for UserTier {
    type Err = crate::error::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "free" => Ok(UserTier::Free),
            "premium" => Ok(UserTier::Premium),
            "unlimited" => Ok(UserTier::Unlimited),
            other => Err(crate::error::Error::Constraint(format!(
                "unknown tier: {other}"
            ))),
        }
    }
}

/// A registered bot user.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct User {
    pub id: i64,
    /// Telegram numeric user ID.
    pub telegram_id: i64,
    /// Which bot this user belongs to (allows multi-bot deployments).
    pub bot_id: String,
    /// Optional Telegram username (without `@`).
    pub username: Option<String>,
    /// Subscription tier string as stored in SQLite (`free` / `premium` / `unlimited`).
    pub tier: String,
    /// Maximum number of active subscriptions allowed for this user.
    pub max_subscriptions: i32,
    /// IANA timezone string, e.g. `"America/New_York"`.
    pub timezone: String,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

impl User {
    /// Parse the `tier` string into the [`UserTier`] enum.
    pub fn parsed_tier(&self) -> crate::error::Result<UserTier> {
        self.tier.parse()
    }
}

// ---------------------------------------------------------------------------
// markets
// ---------------------------------------------------------------------------

/// A Polymarket prediction market.
///
/// The three JSON columns (`outcomes`, `token_ids`, `last_prices`) are stored
/// as plain `TEXT` in SQLite and must be deserialized by the caller when
/// needed.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Market {
    pub id: i64,
    /// Polymarket condition ID (hex string), unique across markets.
    pub condition_id: String,
    /// Human-readable question text.
    pub question: String,
    /// Optional URL slug.
    pub slug: Option<String>,
    /// JSON array of outcome labels, e.g. `["Yes","No"]`.
    pub outcomes: String,
    /// JSON array of CLOB token IDs corresponding to each outcome.
    pub token_ids: String,
    /// JSON array of last-known decimal prices for each outcome.
    pub last_prices: String,
    /// Whether the market is still accepting orders.
    pub is_active: bool,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

impl Market {
    /// Deserialize the `outcomes` JSON column into a `Vec<String>`.
    pub fn parse_outcomes(&self) -> crate::error::Result<Vec<String>> {
        serde_json::from_str(&self.outcomes).map_err(crate::error::Error::Serialization)
    }

    /// Deserialize the `token_ids` JSON column into a `Vec<String>`.
    pub fn parse_token_ids(&self) -> crate::error::Result<Vec<String>> {
        serde_json::from_str(&self.token_ids).map_err(crate::error::Error::Serialization)
    }

    /// Deserialize the `last_prices` JSON column into a `Vec<rust_decimal::Decimal>`.
    pub fn parse_last_prices(&self) -> crate::error::Result<Vec<rust_decimal::Decimal>> {
        serde_json::from_str(&self.last_prices).map_err(crate::error::Error::Serialization)
    }
}

// ---------------------------------------------------------------------------
// subscriptions
// ---------------------------------------------------------------------------

/// A user's subscription to a specific outcome of a market.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Subscription {
    pub id: i64,
    pub user_id: i64,
    pub market_id: i64,
    /// Index into the market's outcomes array (0 = Yes, 1 = No for binary markets).
    pub outcome_index: i32,
    pub is_active: bool,
    pub created_at: NaiveDateTime,
}

// ---------------------------------------------------------------------------
// alerts
// ---------------------------------------------------------------------------

/// The condition that triggers an alert.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
pub enum AlertType {
    /// Trigger when the price rises above `threshold`.
    Above,
    /// Trigger when the price falls below `threshold`.
    Below,
    /// Trigger when the price crosses `threshold` in either direction.
    Cross,
}

impl std::fmt::Display for AlertType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AlertType::Above => write!(f, "above"),
            AlertType::Below => write!(f, "below"),
            AlertType::Cross => write!(f, "cross"),
        }
    }
}

impl std::str::FromStr for AlertType {
    type Err = crate::error::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "above" => Ok(AlertType::Above),
            "below" => Ok(AlertType::Below),
            "cross" => Ok(AlertType::Cross),
            other => Err(crate::error::Error::Constraint(format!(
                "unknown alert_type: {other}"
            ))),
        }
    }
}

/// A price alert attached to a subscription.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Alert {
    pub id: i64,
    pub subscription_id: i64,
    /// Alert condition as stored in SQLite.
    pub alert_type: String,
    /// Price threshold stored as an SQLite `REAL`.
    pub threshold: f64,
    pub is_triggered: bool,
    /// How many minutes must pass before this alert may fire again.
    pub cooldown_minutes: i32,
    pub last_triggered_at: Option<NaiveDateTime>,
    pub created_at: NaiveDateTime,
}

impl Alert {
    /// Parse the `alert_type` string into the [`AlertType`] enum.
    pub fn parsed_alert_type(&self) -> crate::error::Result<AlertType> {
        self.alert_type.parse()
    }
}

// ---------------------------------------------------------------------------
// notification_log
// ---------------------------------------------------------------------------

/// Broad category of a notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT")]
pub enum NotificationLogType {
    #[sqlx(rename = "alert")]
    Alert,
    #[sqlx(rename = "daily_summary")]
    DailySummary,
}

/// A record of every notification attempted (delivered or failed).
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct NotificationLog {
    pub id: i64,
    pub user_id: i64,
    /// The alert that caused this notification, if any.
    pub alert_id: Option<i64>,
    /// Notification category as stored in SQLite.
    pub notification_type: String,
    /// Full message text that was (or was attempted to be) sent.
    pub message: String,
    pub delivered: bool,
    /// Last error message if delivery failed.
    pub error_message: Option<String>,
    pub created_at: NaiveDateTime,
}

// ---------------------------------------------------------------------------
// feedback
// ---------------------------------------------------------------------------

/// A user-submitted feedback message.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Feedback {
    pub id: i64,
    pub user_id: i64,
    pub message: String,
    pub created_at: NaiveDateTime,
}

// ---------------------------------------------------------------------------
// Joined / enriched types used by query helpers
// ---------------------------------------------------------------------------

/// Flattened view of a subscription together with the owning user's Telegram
/// identity and the subscribed market.  Used by the alert engine to evaluate
/// every active alert without additional per-row lookups.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct SubscriptionDetail {
    // subscription fields
    pub subscription_id: i64,
    pub outcome_index: i32,
    // market fields
    pub market_id: i64,
    pub condition_id: String,
    pub question: String,
    pub token_ids: String,
    pub last_prices: String,
    // user fields
    pub user_id: i64,
    pub telegram_id: i64,
    pub bot_id: String,
    pub timezone: String,
}
