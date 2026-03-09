//! Stateful dialogue flows for multi-step bot interactions.
//!
//! Teloxide's dialogue system is used to maintain per-user conversation
//! state across multiple message exchanges.  State is stored in process
//! memory using [`InMemStorage`]; it is intentionally not persisted to the
//! database so that a bot restart cleanly resets any in-progress flow.
//!
//! # Flows
//!
//! * **Subscribe** – search for a market, select it, select an outcome,
//!   then create the subscription row in the database.
//! * **Alert** – choose an existing subscription, pick a condition type
//!   (above / below / cross), enter a threshold, then create the alert row.

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
    // Subscribe flow
    // ------------------------------------------------------------------
    /// Waiting for the user to type a search query.
    AwaitingSearchQuery,

    /// Search results have been displayed; waiting for the user to tap a
    /// market from the inline keyboard.
    AwaitingMarketSelection {
        markets: Vec<MarketOption>,
    },

    /// Market has been chosen; waiting for the user to select an outcome.
    AwaitingOutcomeSelection {
        condition_id: String,
        question: String,
        outcomes: Vec<String>,
        token_ids: Vec<String>,
    },

    // ------------------------------------------------------------------
    // Alert flow
    // ------------------------------------------------------------------
    /// Waiting for the user to choose which subscription to alert on.
    AwaitingAlertSubscription,

    /// Subscription has been chosen; waiting for the user to select an
    /// alert type.
    AwaitingAlertType {
        subscription_id: i64,
        question: String,
    },

    /// Alert type has been chosen; waiting for the user to enter a numeric
    /// threshold.
    AwaitingAlertThreshold {
        subscription_id: i64,
        question: String,
        alert_type: String,
    },
}

// ---------------------------------------------------------------------------
// Subscribe flow handlers
// ---------------------------------------------------------------------------

/// Entry point for the `/subscribe` command.
///
/// Transitions the dialogue to [`DialogueState::AwaitingSearchQuery`] and
/// prompts the user.
#[instrument(skip_all)]
pub async fn handle_subscribe_start(
    bot: Bot,
    dialogue: MyDialogue,
    msg: Message,
) -> anyhow::Result<()> {
    dialogue.update(DialogueState::AwaitingSearchQuery).await?;
    bot.send_message(msg.chat.id, "Enter a search query to find a market:")
        .await?;
    Ok(())
}

/// Receives the search query and shows matching markets as an inline keyboard.
#[instrument(skip_all)]
pub async fn handle_search_query(
    bot: Bot,
    dialogue: MyDialogue,
    msg: Message,
    gamma_client: Arc<GammaClient>,
) -> anyhow::Result<()> {
    let query = match msg.text() {
        Some(t) => t.trim().to_string(),
        None => {
            bot.send_message(msg.chat.id, "Please send a text message with your search query.")
                .await?;
            return Ok(());
        }
    };

    if query.is_empty() {
        bot.send_message(msg.chat.id, "Search query cannot be empty. Try again:").await?;
        return Ok(());
    }

    bot.send_message(msg.chat.id, format!("Searching for \"{query}\"…")).await?;

    let gamma_markets = match gamma_client.search_markets(&query).await {
        Ok(m) => m,
        Err(e) => {
            warn!(error=%e, "Gamma search failed");
            bot.send_message(msg.chat.id, "Search failed. Please try again later.")
                .await?;
            return Ok(());
        }
    };

    if gamma_markets.is_empty() {
        bot.send_message(
            msg.chat.id,
            "No active markets found for that query. Try a different search term.",
        )
        .await?;
        // Stay in AwaitingSearchQuery so the user can try another query.
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
///
/// Advances the dialogue to [`DialogueState::AwaitingOutcomeSelection`].
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

/// Handles an `"outcome:{index}"` callback from the outcome-selection keyboard.
///
/// Upserts the market in the database, checks the user's quota, and inserts
/// the subscription row.
#[instrument(skip_all)]
pub async fn handle_outcome_selection(
    bot: Bot,
    dialogue: MyDialogue,
    q: CallbackQuery,
    pool: SqlitePool,
    bot_id: String,
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

    let telegram_id = q.from.id.0 as i64;
    let username = q.from.username.as_deref();

    // Ensure the user exists.
    let user = get_or_create_user(&pool, telegram_id, &bot_id, username).await?;

    // Quota check.
    let active_count = count_active_subscriptions(&pool, user.id).await?;
    if active_count >= user.max_subscriptions as i64 {
        bot.send_message(
            chat_id,
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
    let result = sqlx::query(
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

    match result {
        Ok(r) if r.rows_affected() == 0 => {
            bot.send_message(
                chat_id,
                format!("You are already subscribed to {outcome_label} on:\n{question}"),
            )
            .await?;
        }
        Ok(_) => {
            bot.send_message(
                chat_id,
                format!(
                    "Subscribed to {outcome_label} on:\n{question}\n\n\
                     Use /alert to set a price alert."
                ),
            )
            .await?;
        }
        Err(e) => {
            warn!(error=%e, "failed to insert subscription");
            bot.send_message(chat_id, "Failed to create subscription. Please try again.")
                .await?;
        }
    }

    dialogue.update(DialogueState::Idle).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Alert flow handlers
// ---------------------------------------------------------------------------

/// Entry point for the `/alert` command.
///
/// Shows the user's subscriptions as an inline keyboard so they can choose
/// which one to set an alert on.
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
        "above" => "Enter the price threshold (0–1). Alert fires when price rises ABOVE it:",
        "below" => "Enter the price threshold (0–1). Alert fires when price falls BELOW it:",
        "cross" => "Enter the price threshold (0–1). Alert fires when price CROSSES it:",
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

/// Receives the threshold value and inserts the alert row.
#[instrument(skip_all)]
pub async fn handle_alert_threshold(
    bot: Bot,
    dialogue: MyDialogue,
    msg: Message,
    pool: SqlitePool,
    subscription_id: i64,
    question: String,
    alert_type: String,
) -> anyhow::Result<()> {
    let text = match msg.text() {
        Some(t) => t.trim().to_string(),
        None => {
            bot.send_message(msg.chat.id, "Please send a numeric value (e.g. 0.70).")
                .await?;
            return Ok(());
        }
    };

    let threshold: f64 = match text.parse() {
        Ok(v) if (0.0..=1.0).contains(&v) => v,
        Ok(_) => {
            bot.send_message(msg.chat.id, "Threshold must be between 0 and 1. Try again:")
                .await?;
            return Ok(());
        }
        Err(_) => {
            bot.send_message(msg.chat.id, "Invalid number. Please enter a decimal like 0.65:")
                .await?;
            return Ok(());
        }
    };

    let result = sqlx::query(
        r#"
        INSERT INTO alerts (subscription_id, alert_type, threshold)
        VALUES (?, ?, ?)
        "#,
    )
    .bind(subscription_id)
    .bind(&alert_type)
    .bind(threshold)
    .execute(&pool)
    .await;

    match result {
        Ok(_) => {
            bot.send_message(
                msg.chat.id,
                format!(
                    "Alert created!\n\
                     Market: {question}\n\
                     Type: {alert_type}\n\
                     Threshold: {threshold:.4}"
                ),
            )
            .await?;
        }
        Err(e) => {
            warn!(error=%e, "failed to insert alert");
            bot.send_message(msg.chat.id, "Failed to create alert. Please try again.")
                .await?;
        }
    }

    dialogue.update(DialogueState::Idle).await?;
    Ok(())
}
