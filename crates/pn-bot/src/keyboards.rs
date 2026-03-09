//! Inline keyboard builders for the Telegram bot.
//!
//! Each function returns an [`InlineKeyboardMarkup`] that is attached to a
//! message.  When the user taps a button teloxide delivers a
//! `CallbackQuery` whose `data` field follows the format documented on each
//! builder.

use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

use crate::dialogues::MarketOption;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum label length before truncation with an ellipsis.
const MAX_LABEL_LEN: usize = 40;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn truncate(s: &str) -> String {
    if s.chars().count() <= MAX_LABEL_LEN {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(MAX_LABEL_LEN - 1).collect();
        format!("{truncated}…")
    }
}

fn button(label: impl Into<String>, data: impl Into<String>) -> InlineKeyboardButton {
    InlineKeyboardButton::callback(label.into(), data.into())
}

// ---------------------------------------------------------------------------
// Keyboard builders
// ---------------------------------------------------------------------------

/// Keyboard listing search-result markets.
///
/// Callback data format: `"market:{index}"` where `index` is the
/// zero-based position in the `markets` slice.
pub fn market_list_keyboard(markets: &[MarketOption]) -> InlineKeyboardMarkup {
    let rows: Vec<Vec<InlineKeyboardButton>> = markets
        .iter()
        .enumerate()
        .map(|(i, m)| vec![button(truncate(&m.question), format!("market:{i}"))])
        .collect();

    InlineKeyboardMarkup::new(rows)
}

/// Keyboard listing outcome labels for a single market.
///
/// Callback data format: `"outcome:{index}"`.
pub fn outcome_keyboard(outcomes: &[String]) -> InlineKeyboardMarkup {
    let rows: Vec<Vec<InlineKeyboardButton>> = outcomes
        .iter()
        .enumerate()
        .map(|(i, label)| vec![button(label.clone(), format!("outcome:{i}"))])
        .collect();

    InlineKeyboardMarkup::new(rows)
}

/// Keyboard with the three alert condition types.
///
/// Callback data format: `"alert_type:{type}"` where type is one of
/// `above`, `below`, `cross`.
pub fn alert_type_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![
        button("Above threshold", "alert_type:above"),
        button("Below threshold", "alert_type:below"),
        button("Cross threshold", "alert_type:cross"),
    ]])
}

/// Keyboard listing user subscriptions for selection.
///
/// `subscriptions` is a slice of `(subscription_id, display_label)` pairs.
///
/// Callback data format: `"sub:{id}"`.
pub fn subscription_list_keyboard(subscriptions: &[(i64, String)]) -> InlineKeyboardMarkup {
    let rows: Vec<Vec<InlineKeyboardButton>> = subscriptions
        .iter()
        .map(|(id, label)| vec![button(truncate(label), format!("sub:{id}"))])
        .collect();

    InlineKeyboardMarkup::new(rows)
}

/// Keyboard listing user subscriptions for *unsubscribing*.
///
/// Callback data format: `"unsub:{id}"`.
pub fn unsubscribe_keyboard(subscriptions: &[(i64, String)]) -> InlineKeyboardMarkup {
    let rows: Vec<Vec<InlineKeyboardButton>> = subscriptions
        .iter()
        .map(|(id, label)| vec![button(truncate(label), format!("unsub:{id}"))])
        .collect();

    InlineKeyboardMarkup::new(rows)
}

/// Keyboard listing user alerts for removal.
///
/// `alerts` is a slice of `(alert_id, display_label)` pairs.
///
/// Callback data format: `"rm_alert:{id}"`.
pub fn alert_list_keyboard(alerts: &[(i64, String)]) -> InlineKeyboardMarkup {
    let rows: Vec<Vec<InlineKeyboardButton>> = alerts
        .iter()
        .map(|(id, label)| vec![button(truncate(label), format!("rm_alert:{id}"))])
        .collect();

    InlineKeyboardMarkup::new(rows)
}

/// Simple Yes/No confirmation keyboard.
///
/// Callback data: `"confirm:yes"` or `"confirm:no"`.
pub fn confirm_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![
        button("Yes", "confirm:yes"),
        button("No", "confirm:no"),
    ]])
}
