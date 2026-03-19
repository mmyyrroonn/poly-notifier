//! Stateful dialogue flows for multi-step bot interactions.
//!
//! Teloxide's dialogue system is used to maintain per-user conversation
//! state across multiple message exchanges.  State is stored in process
//! memory using [`InMemStorage`]; it is intentionally not persisted to the
//! database so that a bot restart cleanly resets any in-progress flow.
//!
//! # Flows
//!
//! * **Subscribe** – paste a Polymarket URL (or search query), select a
//!   market/outcome, choose an alert type and threshold, then create the
//!   subscription *and* alert row in one go.
//! * **Alert** – modify or add alerts on an existing subscription.

use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use std::sync::Arc;
use teloxide::{
    dispatching::dialogue::{Dialogue, InMemStorage},
    prelude::*,
};
use tracing::{debug, instrument, warn};

use pn_common::db::{count_active_subscriptions, get_or_create_user, get_user_subscriptions};
use pn_polymarket::GammaClient;

use crate::keyboards::{
    alert_type_keyboard, market_list_keyboard, outcome_keyboard,
    subscription_list_keyboard,
};

// ---------------------------------------------------------------------------
// Type aliases
// ---------------------------------------------------------------------------

/// The concrete dialogue type used throughout the bot.
pub type MyDialogue = Dialogue<DialogueState, InMemStorage<DialogueState>>;

// ---------------------------------------------------------------------------
// Shared data structures
// ---------------------------------------------------------------------------

/// A single market option presented to the user during market search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketOption {
    pub condition_id: String,
    pub question: String,
    pub outcomes: Vec<String>,
    pub token_ids: Vec<String>,
}

// ---------------------------------------------------------------------------
// Dialogue state machine
// ---------------------------------------------------------------------------

/// Every possible state a conversation may be in.
///
/// The `#[default]` attribute on `Idle` means a fresh conversation or one
/// that has been reset always starts here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum DialogueState {
    /// No active flow – the bot is waiting for a top-level command.
    #[default]
    Idle,

    // ------------------------------------------------------------------
    // Subscribe flow (URL → market → outcome → alert type → threshold)
    // ------------------------------------------------------------------
    /// Waiting for the user to paste a Polymarket URL or search query.
    AwaitingUrl,

    /// Multiple markets found; waiting for the user to tap one.
    AwaitingMarketSelection {
        markets: Vec<MarketOption>,
    },

    /// Market chosen; waiting for the user to select an outcome.
    AwaitingOutcomeSelection {
        condition_id: String,
        question: String,
        outcomes: Vec<String>,
        token_ids: Vec<String>,
    },

    /// Outcome chosen; waiting for alert type selection (subscribe flow).
    AwaitingSubscribeAlertType {
        condition_id: String,
        question: String,
        outcomes: Vec<String>,
        token_ids: Vec<String>,
        outcome_index: i64,
    },

    /// Alert type chosen; waiting for threshold input (subscribe flow).
    AwaitingSubscribeAlertThreshold {
        condition_id: String,
        question: String,
        outcomes: Vec<String>,
        token_ids: Vec<String>,
        outcome_index: i64,
        alert_type: String,
    },

    // ------------------------------------------------------------------
    // Alert flow (modify/add alert on existing subscription)
    // ------------------------------------------------------------------
    /// Waiting for the user to choose which subscription to alert on.
    AwaitingAlertSubscription,

    /// Subscription chosen; waiting for alert type selection.
    AwaitingAlertType {
        subscription_id: i64,
        question: String,
    },

    /// Alert type chosen; waiting for threshold input.
    AwaitingAlertThreshold {
        subscription_id: i64,
        question: String,
        alert_type: String,
    },
}

// ---------------------------------------------------------------------------
// URL slug extraction
// ---------------------------------------------------------------------------

/// Extract the event slug from a Polymarket URL.
///
/// Supports formats:
/// - `https://polymarket.com/event/{slug}`
/// - `https://polymarket.com/event/{slug}/{market-slug}`
/// - `polymarket.com/event/{slug}`
///
/// Returns `None` if the text is not a recognisable Polymarket URL.
pub fn extract_slug_from_url(text: &str) -> Option<String> {
    let text = text.trim();
    // Find "/event/" in the URL
    let idx = text.find("/event/")?;
    let after = &text[idx + "/event/".len()..];
    // Take the first path segment (up to '/' or '?' or end)
    let slug = after
        .split(&['/', '?', '#'][..])
        .next()?;
    if slug.is_empty() {
        return None;
    }
    Some(slug.to_string())
}

// ---------------------------------------------------------------------------
// Subscribe flow handlers
// ---------------------------------------------------------------------------

/// Entry point for the `/subscribe` command.
#[instrument(skip_all)]
pub async fn handle_subscribe_start(
    bot: Bot,
    dialogue: MyDialogue,
    msg: Message,
) -> anyhow::Result<()> {
    dialogue.update(DialogueState::AwaitingUrl).await?;
    bot.send_message(
        msg.chat.id,
        "Paste a Polymarket event URL (e.g. https://polymarket.com/event/…)\n\
         or type a search query:",
    )
    .await?;
    Ok(())
}

/// Receives a URL or search query and shows matching markets.
#[instrument(skip_all)]
pub async fn handle_url_input(
    bot: Bot,
    dialogue: MyDialogue,
    msg: Message,
    gamma_client: Arc<GammaClient>,
) -> anyhow::Result<()> {
    let text = match msg.text() {
        Some(t) => t.trim().to_string(),
        None => {
            bot.send_message(msg.chat.id, "Please send a text message.")
                .await?;
            return Ok(());
        }
    };

    if text.is_empty() {
        bot.send_message(msg.chat.id, "Input cannot be empty. Try again:")
            .await?;
        return Ok(());
    }

    bot.send_message(msg.chat.id, "Searching…").await?;

    // Try URL slug extraction first, otherwise treat as search query.
    let gamma_markets = if let Some(slug) = extract_slug_from_url(&text) {
        match gamma_client.get_market_by_slug(&slug).await {
            Ok(m) => m,
            Err(e) => {
                warn!(error=%e, "Gamma slug lookup failed");
                bot.send_message(msg.chat.id, "Search failed. Please try again later.")
                    .await?;
                return Ok(());
            }
        }
    } else {
        match gamma_client.search_markets(&text).await {
            Ok(m) => m,
            Err(e) => {
                warn!(error=%e, "Gamma search failed");
                bot.send_message(msg.chat.id, "Search failed. Please try again later.")
                    .await?;
                return Ok(());
            }
        }
    };

    if gamma_markets.is_empty() {
        bot.send_message(
            msg.chat.id,
            "No active markets found. Try a different URL or search term.",
        )
        .await?;
        return Ok(());
    }

    let options: Vec<MarketOption> = gamma_markets
        .into_iter()
        .take(10)
        .map(|m| {
            let outcomes: Vec<String> = m.tokens.iter().map(|t| t.outcome.clone()).collect();
            let token_ids: Vec<String> = m.tokens.iter().map(|t| t.token_id.clone()).collect();
            MarketOption {
                condition_id: m.condition_id,
                question: m.question,
                outcomes,
                token_ids,
            }
        })
        .collect();

    // If there's exactly one market, skip straight to outcome selection.
    if options.len() == 1 {
        let market = &options[0];
        let kb = outcome_keyboard(&market.outcomes);
        bot.send_message(
            msg.chat.id,
            format!("Market: {}\n\nSelect an outcome:", market.question),
        )
        .reply_markup(kb)
        .await?;

        dialogue
            .update(DialogueState::AwaitingOutcomeSelection {
                condition_id: market.condition_id.clone(),
                question: market.question.clone(),
                outcomes: market.outcomes.clone(),
                token_ids: market.token_ids.clone(),
            })
            .await?;
        return Ok(());
    }

    let kb = market_list_keyboard(&options);
    bot.send_message(msg.chat.id, "Select a market:")
        .reply_markup(kb)
        .await?;

    dialogue
        .update(DialogueState::AwaitingMarketSelection { markets: options })
        .await?;
    Ok(())
}

/// Handles a `"market:{index}"` callback from the market-selection keyboard.
#[instrument(skip_all)]
pub async fn handle_market_selection(
    bot: Bot,
    dialogue: MyDialogue,
    q: CallbackQuery,
    markets: Vec<MarketOption>,
) -> anyhow::Result<()> {
    bot.answer_callback_query(q.id.clone()).await?;

    let data = match q.data.as_deref() {
        Some(d) => d,
        None => return Ok(()),
    };

    let index: usize = match data.strip_prefix("market:").and_then(|s| s.parse().ok()) {
        Some(i) => i,
        None => {
            debug!(data, "unexpected callback data in market selection");
            return Ok(());
        }
    };

    let market = match markets.get(index) {
        Some(m) => m.clone(),
        None => {
            if let Some(msg) = q.message {
                bot.send_message(msg.chat().id, "Invalid selection. Please try /subscribe again.")
                    .await?;
            }
            dialogue.update(DialogueState::Idle).await?;
            return Ok(());
        }
    };

    let kb = outcome_keyboard(&market.outcomes);

    if let Some(msg) = q.message {
        bot.send_message(
            msg.chat().id,
            format!("Market: {}\n\nSelect an outcome:", market.question),
        )
        .reply_markup(kb)
        .await?;

        dialogue
            .update(DialogueState::AwaitingOutcomeSelection {
                condition_id: market.condition_id,
                question: market.question,
                outcomes: market.outcomes,
                token_ids: market.token_ids,
            })
            .await?;
    }

    Ok(())
}

/// Handles an `"outcome:{index}"` callback – advances to alert type selection.
#[instrument(skip_all)]
pub async fn handle_outcome_selection(
    bot: Bot,
    dialogue: MyDialogue,
    q: CallbackQuery,
    condition_id: String,
    question: String,
    outcomes: Vec<String>,
    token_ids: Vec<String>,
) -> anyhow::Result<()> {
    bot.answer_callback_query(q.id.clone()).await?;

    let data = match q.data.as_deref() {
        Some(d) => d,
        None => return Ok(()),
    };

    let outcome_index: i64 = match data.strip_prefix("outcome:").and_then(|s| s.parse().ok()) {
        Some(i) => i,
        None => {
            debug!(data, "unexpected callback data in outcome selection");
            return Ok(());
        }
    };

    let chat_id = match q.message.as_ref() {
        Some(m) => m.chat().id,
        None => return Ok(()),
    };

    let outcome_label = outcomes
        .get(outcome_index as usize)
        .cloned()
        .unwrap_or_else(|| format!("Outcome {outcome_index}"));

    let kb = alert_type_keyboard();
    bot.send_message(
        chat_id,
        format!(
            "Market: {question}\nOutcome: {outcome_label}\n\nSelect alert type:"
        ),
    )
    .reply_markup(kb)
    .await?;

    dialogue
        .update(DialogueState::AwaitingSubscribeAlertType {
            condition_id,
            question,
            outcomes,
            token_ids,
            outcome_index,
        })
        .await?;
    Ok(())
}

/// Handles `"alert_type:{type}"` callback in the subscribe flow.
#[instrument(skip_all)]
#[allow(clippy::too_many_arguments)]
pub async fn handle_subscribe_alert_type_selection(
    bot: Bot,
    dialogue: MyDialogue,
    q: CallbackQuery,
    condition_id: String,
    question: String,
    outcomes: Vec<String>,
    token_ids: Vec<String>,
    outcome_index: i64,
) -> anyhow::Result<()> {
    bot.answer_callback_query(q.id.clone()).await?;

    let data = match q.data.as_deref() {
        Some(d) => d,
        None => return Ok(()),
    };

    let alert_type = match data.strip_prefix("alert_type:") {
        Some(t) if matches!(t, "above" | "below" | "cross") => t.to_string(),
        _ => return Ok(()),
    };

    let chat_id = match q.message.as_ref() {
        Some(m) => m.chat().id,
        None => return Ok(()),
    };

    let prompt = match alert_type.as_str() {
        "above" => "Enter the price threshold (0–100%). Alert fires when price rises ABOVE it:",
        "below" => "Enter the price threshold (0–100%). Alert fires when price falls BELOW it:",
        "cross" => "Enter the price threshold (0–100%). Alert fires when price CROSSES it:",
        _ => unreachable!(),
    };

    bot.send_message(chat_id, prompt).await?;

    dialogue
        .update(DialogueState::AwaitingSubscribeAlertThreshold {
            condition_id,
            question,
            outcomes,
            token_ids,
            outcome_index,
            alert_type,
        })
        .await?;
    Ok(())
}

/// Receives the threshold (0-100), creates both subscription and alert.
#[instrument(skip_all)]
#[allow(clippy::too_many_arguments)]
pub async fn handle_subscribe_alert_threshold(
    bot: Bot,
    dialogue: MyDialogue,
    msg: Message,
    pool: SqlitePool,
    bot_id: String,
    condition_id: String,
    question: String,
    outcomes: Vec<String>,
    token_ids: Vec<String>,
    outcome_index: i64,
    alert_type: String,
) -> anyhow::Result<()> {
    let text = match msg.text() {
        Some(t) => t.trim().to_string(),
        None => {
            bot.send_message(msg.chat.id, "Please send a number (e.g. 70).")
                .await?;
            return Ok(());
        }
    };

    let pct: f64 = match text.parse() {
        Ok(v) if (0.0..=100.0).contains(&v) => v,
        Ok(_) => {
            bot.send_message(msg.chat.id, "Threshold must be between 0 and 100. Try again:")
                .await?;
            return Ok(());
        }
        Err(_) => {
            bot.send_message(msg.chat.id, "Invalid number. Please enter a value like 70:")
                .await?;
            return Ok(());
        }
    };

    let threshold = pct / 100.0;
    let telegram_id = match msg.from.as_ref() {
        Some(u) => u.id.0 as i64,
        None => return Ok(()),
    };
    let username = msg.from.as_ref().and_then(|u| u.username.as_deref());

    let user = get_or_create_user(&pool, telegram_id, &bot_id, username).await?;

    // Quota check.
    let active_count = count_active_subscriptions(&pool, user.id).await?;
    if active_count >= user.max_subscriptions as i64 {
        bot.send_message(
            msg.chat.id,
            format!(
                "You have reached your subscription limit ({max}). \
                 Unsubscribe from a market first.",
                max = user.max_subscriptions
            ),
        )
        .await?;
        dialogue.update(DialogueState::Idle).await?;
        return Ok(());
    }

    // Upsert market.
    let outcomes_json = serde_json::to_string(&outcomes)?;
    let token_ids_json = serde_json::to_string(&token_ids)?;
    let market_id = pn_common::db::upsert_market(
        &pool,
        &condition_id,
        &question,
        None,
        &outcomes_json,
        &token_ids_json,
    )
    .await?;

    let outcome_label = outcomes
        .get(outcome_index as usize)
        .cloned()
        .unwrap_or_else(|| format!("Outcome {outcome_index}"));

    // Insert subscription (ignore duplicate).
    let sub_result = sqlx::query(
        r#"
        INSERT OR IGNORE INTO subscriptions (user_id, market_id, outcome_index)
        VALUES (?, ?, ?)
        "#,
    )
    .bind(user.id)
    .bind(market_id)
    .bind(outcome_index)
    .execute(&pool)
    .await;

    match sub_result {
        Ok(r) if r.rows_affected() == 0 => {
            bot.send_message(
                msg.chat.id,
                format!("You are already subscribed to {outcome_label} on:\n{question}"),
            )
            .await?;
            dialogue.update(DialogueState::Idle).await?;
            return Ok(());
        }
        Ok(_) => {}
        Err(e) => {
            warn!(error=%e, "failed to insert subscription");
            bot.send_message(msg.chat.id, "Failed to create subscription. Please try again.")
                .await?;
            dialogue.update(DialogueState::Idle).await?;
            return Ok(());
        }
    }

    // Get the subscription ID we just created.
    let sub_row: Option<(i64,)> = sqlx::query_as(
        "SELECT id FROM subscriptions WHERE user_id = ? AND market_id = ? AND outcome_index = ? AND is_active = 1",
    )
    .bind(user.id)
    .bind(market_id)
    .bind(outcome_index)
    .fetch_optional(&pool)
    .await?;

    let subscription_id = match sub_row {
        Some((id,)) => id,
        None => {
            bot.send_message(msg.chat.id, "Failed to find subscription. Please try again.")
                .await?;
            dialogue.update(DialogueState::Idle).await?;
            return Ok(());
        }
    };

    // Insert alert.
    let alert_result = sqlx::query(
        "INSERT INTO alerts (subscription_id, alert_type, threshold) VALUES (?, ?, ?)",
    )
    .bind(subscription_id)
    .bind(&alert_type)
    .bind(threshold)
    .execute(&pool)
    .await;

    match alert_result {
        Ok(_) => {
            bot.send_message(
                msg.chat.id,
                format!(
                    "Subscribed with alert!\n\
                     Market: {question}\n\
                     Outcome: {outcome_label}\n\
                     Alert: {alert_type} {pct:.0}%"
                ),
            )
            .await?;
        }
        Err(e) => {
            warn!(error=%e, "failed to insert alert");
            bot.send_message(
                msg.chat.id,
                format!(
                    "Subscribed to {outcome_label} on:\n{question}\n\n\
                     ⚠️ Failed to create alert. Use /alert to add one."
                ),
            )
            .await?;
        }
    }

    dialogue.update(DialogueState::Idle).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Alert flow handlers (modify/add alerts on existing subscriptions)
// ---------------------------------------------------------------------------

/// Entry point for the `/alert` command.
#[instrument(skip_all)]
pub async fn handle_alert_start(
    bot: Bot,
    dialogue: MyDialogue,
    msg: Message,
    pool: SqlitePool,
    bot_id: String,
) -> anyhow::Result<()> {
    let telegram_id = match msg.from.as_ref() {
        Some(u) => u.id.0 as i64,
        None => return Ok(()),
    };

    let user =
        get_or_create_user(&pool, telegram_id, &bot_id, None).await?;
    let subs = get_user_subscriptions(&pool, user.id).await?;

    if subs.is_empty() {
        bot.send_message(
            msg.chat.id,
            "You have no active subscriptions. Use /subscribe first.",
        )
        .await?;
        return Ok(());
    }

    let items: Vec<(i64, String)> = subs
        .iter()
        .map(|s| {
            let label = format!("{} (outcome {})", s.question, s.outcome_index);
            (s.subscription_id, label)
        })
        .collect();

    let kb = subscription_list_keyboard(&items);

    bot.send_message(msg.chat.id, "Select a subscription to set an alert on:")
        .reply_markup(kb)
        .await?;

    dialogue
        .update(DialogueState::AwaitingAlertSubscription)
        .await?;
    Ok(())
}

/// Handles a `"sub:{id}"` callback when the user picks a subscription.
#[instrument(skip_all)]
pub async fn handle_alert_subscription_selection(
    bot: Bot,
    dialogue: MyDialogue,
    q: CallbackQuery,
    pool: SqlitePool,
) -> anyhow::Result<()> {
    bot.answer_callback_query(q.id.clone()).await?;

    let data = match q.data.as_deref() {
        Some(d) => d,
        None => return Ok(()),
    };

    let sub_id: i64 = match data.strip_prefix("sub:").and_then(|s| s.parse().ok()) {
        Some(i) => i,
        None => return Ok(()),
    };

    let chat_id = match q.message.as_ref() {
        Some(m) => m.chat().id,
        None => return Ok(()),
    };

    // Fetch the subscription's market question.
    let row = sqlx::query(
        r#"
        SELECT m.question
        FROM subscriptions s
        JOIN markets m ON m.id = s.market_id
        WHERE s.id = ?
        "#,
    )
    .bind(sub_id)
    .fetch_optional(&pool)
    .await?;

    let question = match row {
        Some(r) => {
            let q_str: String = r.get("question");
            q_str
        }
        None => {
            bot.send_message(chat_id, "Subscription not found. Please try /alert again.")
                .await?;
            dialogue.update(DialogueState::Idle).await?;
            return Ok(());
        }
    };

    let kb = alert_type_keyboard();
    bot.send_message(
        chat_id,
        format!("Market: {question}\n\nSelect alert type:"),
    )
    .reply_markup(kb)
    .await?;

    dialogue
        .update(DialogueState::AwaitingAlertType {
            subscription_id: sub_id,
            question,
        })
        .await?;
    Ok(())
}

/// Handles an `"alert_type:{type}"` callback from the alert-type keyboard.
#[instrument(skip_all)]
pub async fn handle_alert_type_selection(
    bot: Bot,
    dialogue: MyDialogue,
    q: CallbackQuery,
    subscription_id: i64,
    question: String,
) -> anyhow::Result<()> {
    bot.answer_callback_query(q.id.clone()).await?;

    let data = match q.data.as_deref() {
        Some(d) => d,
        None => return Ok(()),
    };

    let alert_type = match data.strip_prefix("alert_type:") {
        Some(t) if matches!(t, "above" | "below" | "cross") => t.to_string(),
        _ => return Ok(()),
    };

    let chat_id = match q.message.as_ref() {
        Some(m) => m.chat().id,
        None => return Ok(()),
    };

    let prompt = match alert_type.as_str() {
        "above" => "Enter the price threshold (0–100%). Alert fires when price rises ABOVE it:",
        "below" => "Enter the price threshold (0–100%). Alert fires when price falls BELOW it:",
        "cross" => "Enter the price threshold (0–100%). Alert fires when price CROSSES it:",
        _ => unreachable!(),
    };

    bot.send_message(chat_id, prompt).await?;

    dialogue
        .update(DialogueState::AwaitingAlertThreshold {
            subscription_id,
            question,
            alert_type,
        })
        .await?;
    Ok(())
}

/// Receives the threshold value (0-100%) and inserts/updates the alert.
///
/// For free-tier users: if the subscription already has an alert, UPDATE it;
/// otherwise INSERT (but only if the subscription has 0 alerts).
/// For premium/unlimited users: always INSERT a new alert.
#[instrument(skip_all)]
#[allow(clippy::too_many_arguments)]
pub async fn handle_alert_threshold(
    bot: Bot,
    dialogue: MyDialogue,
    msg: Message,
    pool: SqlitePool,
    bot_id: String,
    subscription_id: i64,
    question: String,
    alert_type: String,
) -> anyhow::Result<()> {
    let text = match msg.text() {
        Some(t) => t.trim().to_string(),
        None => {
            bot.send_message(msg.chat.id, "Please send a number (e.g. 70).")
                .await?;
            return Ok(());
        }
    };

    let pct: f64 = match text.parse() {
        Ok(v) if (0.0..=100.0).contains(&v) => v,
        Ok(_) => {
            bot.send_message(msg.chat.id, "Threshold must be between 0 and 100. Try again:")
                .await?;
            return Ok(());
        }
        Err(_) => {
            bot.send_message(msg.chat.id, "Invalid number. Please enter a value like 70:")
                .await?;
            return Ok(());
        }
    };

    let threshold = pct / 100.0;

    // Get user tier to decide INSERT vs UPDATE.
    let telegram_id = match msg.from.as_ref() {
        Some(u) => u.id.0 as i64,
        None => return Ok(()),
    };
    let user = get_or_create_user(&pool, telegram_id, &bot_id, None).await?;

    // Count existing alerts for this subscription.
    let alert_count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM alerts WHERE subscription_id = ?",
    )
    .bind(subscription_id)
    .fetch_one(&pool)
    .await?;

    let is_free = user.tier == "free";

    if is_free && alert_count.0 > 0 {
        // Update the existing alert.
        let result = sqlx::query(
            "UPDATE alerts SET alert_type = ?, threshold = ? WHERE subscription_id = ? \
             AND id = (SELECT id FROM alerts WHERE subscription_id = ? ORDER BY created_at LIMIT 1)",
        )
        .bind(&alert_type)
        .bind(threshold)
        .bind(subscription_id)
        .bind(subscription_id)
        .execute(&pool)
        .await;

        match result {
            Ok(_) => {
                bot.send_message(
                    msg.chat.id,
                    format!(
                        "Alert updated!\n\
                         Market: {question}\n\
                         Type: {alert_type}\n\
                         Threshold: {pct:.0}%"
                    ),
                )
                .await?;
            }
            Err(e) => {
                warn!(error=%e, "failed to update alert");
                bot.send_message(msg.chat.id, "Failed to update alert. Please try again.")
                    .await?;
            }
        }
    } else if is_free && alert_count.0 == 0 {
        // Free user, no existing alert – INSERT one.
        insert_alert(&bot, &pool, msg.chat.id, subscription_id, &alert_type, threshold, &question, pct).await?;
    } else {
        // Premium/unlimited – always INSERT.
        insert_alert(&bot, &pool, msg.chat.id, subscription_id, &alert_type, threshold, &question, pct).await?;
    }

    dialogue.update(DialogueState::Idle).await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn insert_alert(
    bot: &Bot,
    pool: &SqlitePool,
    chat_id: ChatId,
    subscription_id: i64,
    alert_type: &str,
    threshold: f64,
    question: &str,
    pct: f64,
) -> anyhow::Result<()> {
    let result = sqlx::query(
        "INSERT INTO alerts (subscription_id, alert_type, threshold) VALUES (?, ?, ?)",
    )
    .bind(subscription_id)
    .bind(alert_type)
    .bind(threshold)
    .execute(pool)
    .await;

    match result {
        Ok(_) => {
            bot.send_message(
                chat_id,
                format!(
                    "Alert created!\n\
                     Market: {question}\n\
                     Type: {alert_type}\n\
                     Threshold: {pct:.0}%"
                ),
            )
            .await?;
        }
        Err(e) => {
            warn!(error=%e, "failed to insert alert");
            bot.send_message(chat_id, "Failed to create alert. Please try again.")
                .await?;
        }
    }
    Ok(())
}
