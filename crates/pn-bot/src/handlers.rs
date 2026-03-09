//! Command handler implementations.
//!
//! Each function corresponds to one [`Command`] variant and is called by the
//! teloxide dispatcher after the command text has been parsed.  All handlers
//! use dependency injection via teloxide's [`DependencyMap`]: the pool,
//! `bot_id`, and API clients are stored in the map and extracted by the
//! handler signatures.

use std::sync::Arc;

use sqlx::{Row, SqlitePool};
use teloxide::prelude::*;
use tracing::{instrument, warn};

use pn_common::db::{count_active_subscriptions, get_or_create_user, get_user_subscriptions};
use pn_polymarket::ClobClient;

use teloxide::utils::command::BotCommands;

use crate::{
    commands::Command,
    dialogues::{handle_alert_start, handle_subscribe_start, MyDialogue},
    keyboards::{alert_list_keyboard, unsubscribe_keyboard},
};

// ---------------------------------------------------------------------------
// /start
// ---------------------------------------------------------------------------

/// Create or update the user record and send a welcome message.
#[instrument(skip_all, fields(telegram_id))]
pub async fn start(
    bot: Bot,
    msg: Message,
    pool: SqlitePool,
    bot_id: Arc<String>,
) -> anyhow::Result<()> {
    let from = match msg.from.as_ref() {
        Some(u) => u,
        None => return Ok(()),
    };

    let telegram_id = from.id.0 as i64;
    let username = from.username.as_deref();

    let user = get_or_create_user(&pool, telegram_id, &bot_id, username).await?;
    let used = count_active_subscriptions(&pool, user.id).await?;

    let greeting = match from.first_name.as_str() {
        "" => "Welcome".to_string(),
        name => format!("Welcome, {name}"),
    };

    bot.send_message(
        msg.chat.id,
        format!(
            "{greeting}! I'm the Poly-Notifier bot.\n\n\
             I can notify you about price movements on Polymarket.\n\n\
             Your account:\n\
             Tier: {tier}\n\
             Subscription slots: {used}/{max}\n\n\
             Use /help to see all commands.",
            tier = user.tier,
            max = user.max_subscriptions,
        ),
    )
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// /help
// ---------------------------------------------------------------------------

/// Show all available commands.
pub async fn help(bot: Bot, msg: Message) -> anyhow::Result<()> {
    bot.send_message(msg.chat.id, Command::descriptions().to_string())
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// /list
// ---------------------------------------------------------------------------

/// Display the user's active subscriptions with market names and last prices.
#[instrument(skip_all)]
pub async fn list(
    bot: Bot,
    msg: Message,
    pool: SqlitePool,
    bot_id: Arc<String>,
) -> anyhow::Result<()> {
    let telegram_id = match msg.from.as_ref() {
        Some(u) => u.id.0 as i64,
        None => return Ok(()),
    };

    let user = get_or_create_user(&pool, telegram_id, &bot_id, None).await?;
    let subs = get_user_subscriptions(&pool, user.id).await?;

    if subs.is_empty() {
        bot.send_message(
            msg.chat.id,
            "You have no active subscriptions.\n\nUse /subscribe to add one.",
        )
        .await?;
        return Ok(());
    }

    let mut lines = vec!["Your Subscriptions:".to_string(), String::new()];

    for (i, s) in subs.iter().enumerate() {
        let last_prices: Vec<f64> =
            serde_json::from_str(&s.last_prices).unwrap_or_default();

        let outcome_label = {
            // The token_ids column is a JSON array of token IDs, not labels;
            // we need the outcomes column.  Use a separate query for the label.
            let row = sqlx::query(
                "SELECT outcomes FROM markets WHERE condition_id = ?",
            )
            .bind(&s.condition_id)
            .fetch_optional(&pool)
            .await?;
            let outcome_names: Vec<String> = row
                .and_then(|r: sqlx::sqlite::SqliteRow| {
                    let outcomes: String = r.get("outcomes");
                    serde_json::from_str(&outcomes).ok()
                })
                .unwrap_or_default();
            outcome_names
                .get(s.outcome_index as usize)
                .cloned()
                .unwrap_or_else(|| format!("Outcome {}", s.outcome_index))
        };

        let price_str = last_prices
            .get(s.outcome_index as usize)
            .map(|p| format!("${:.2}", p))
            .unwrap_or_else(|| "N/A".to_string());

        lines.push(format!("{}. {}", i + 1, s.question));
        lines.push(format!("   {} @ {}", outcome_label, price_str));
        lines.push(String::new());
    }

    bot.send_message(msg.chat.id, lines.join("\n")).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// /prices
// ---------------------------------------------------------------------------

/// Fetch current prices from the CLOB API for all subscribed markets.
#[instrument(skip_all)]
pub async fn prices(
    bot: Bot,
    msg: Message,
    pool: SqlitePool,
    bot_id: Arc<String>,
    clob_client: Arc<ClobClient>,
) -> anyhow::Result<()> {
    let telegram_id = match msg.from.as_ref() {
        Some(u) => u.id.0 as i64,
        None => return Ok(()),
    };

    let user = get_or_create_user(&pool, telegram_id, &bot_id, None).await?;
    let subs = get_user_subscriptions(&pool, user.id).await?;

    if subs.is_empty() {
        bot.send_message(
            msg.chat.id,
            "You have no active subscriptions. Use /subscribe to add one.",
        )
        .await?;
        return Ok(());
    }

    bot.send_message(msg.chat.id, "Fetching current prices…").await?;

    let mut lines = vec!["Current Prices:".to_string(), String::new()];

    for (i, s) in subs.iter().enumerate() {
        let token_ids: Vec<String> =
            serde_json::from_str(&s.token_ids).unwrap_or_default();
        let outcome_names: Vec<String> = {
            let row = sqlx::query(
                "SELECT outcomes FROM markets WHERE condition_id = ?",
            )
            .bind(&s.condition_id)
            .fetch_optional(&pool)
            .await?;
            row.and_then(|r: sqlx::sqlite::SqliteRow| {
                let outcomes: String = r.get("outcomes");
                serde_json::from_str(&outcomes).ok()
            })
            .unwrap_or_default()
        };

        let outcome_label = outcome_names
            .get(s.outcome_index as usize)
            .cloned()
            .unwrap_or_else(|| format!("Outcome {}", s.outcome_index));

        let token_id = match token_ids.get(s.outcome_index as usize) {
            Some(t) => t,
            None => {
                lines.push(format!("{}. {} – token not found", i + 1, s.question));
                lines.push(String::new());
                continue;
            }
        };

        let price = match clob_client.get_midpoints(&[token_id.clone()]).await {
            Ok(map) => map
                .get(token_id)
                .map(|p| format!("${:.4}", p))
                .unwrap_or_else(|| "N/A".to_string()),
            Err(e) => {
                warn!(token_id, error=%e, "failed to fetch CLOB midpoints");
                "fetch error".to_string()
            }
        };

        lines.push(format!("{}. {}", i + 1, s.question));
        lines.push(format!("   {} @ {}", outcome_label, price));
        lines.push(String::new());
    }

    bot.send_message(msg.chat.id, lines.join("\n")).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// /unsubscribe
// ---------------------------------------------------------------------------

/// Show an inline keyboard of the user's subscriptions; removal is handled
/// by the callback-query handler in `lib.rs`.
#[instrument(skip_all)]
pub async fn unsubscribe(
    bot: Bot,
    msg: Message,
    pool: SqlitePool,
    bot_id: Arc<String>,
) -> anyhow::Result<()> {
    let telegram_id = match msg.from.as_ref() {
        Some(u) => u.id.0 as i64,
        None => return Ok(()),
    };

    let user = get_or_create_user(&pool, telegram_id, &bot_id, None).await?;
    let subs = get_user_subscriptions(&pool, user.id).await?;

    if subs.is_empty() {
        bot.send_message(msg.chat.id, "You have no active subscriptions.")
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

    let kb = unsubscribe_keyboard(&items);

    bot.send_message(msg.chat.id, "Select a subscription to remove:")
        .reply_markup(kb)
        .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// /remove_alert
// ---------------------------------------------------------------------------

/// Show an inline keyboard of the user's alerts; removal is handled by the
/// callback-query handler in `lib.rs`.
#[instrument(skip_all)]
pub async fn remove_alert(
    bot: Bot,
    msg: Message,
    pool: SqlitePool,
    bot_id: Arc<String>,
) -> anyhow::Result<()> {
    let telegram_id = match msg.from.as_ref() {
        Some(u) => u.id.0 as i64,
        None => return Ok(()),
    };

    let user = get_or_create_user(&pool, telegram_id, &bot_id, None).await?;

    // Fetch all alerts for this user via a join.
    let rows = sqlx::query(
        r#"
        SELECT
            a.id,
            a.alert_type,
            a.threshold,
            m.question,
            s.outcome_index
        FROM alerts a
        JOIN subscriptions s ON s.id = a.subscription_id
        JOIN markets m ON m.id = s.market_id
        WHERE s.user_id = ?
        ORDER BY a.created_at
        "#,
    )
    .bind(user.id)
    .fetch_all(&pool)
    .await?;

    if rows.is_empty() {
        bot.send_message(msg.chat.id, "You have no active alerts.")
            .await?;
        return Ok(());
    }

    let items: Vec<(i64, String)> = rows
        .into_iter()
        .map(|r: sqlx::sqlite::SqliteRow| {
            let id: i64 = r.get("id");
            let alert_type: String = r.get("alert_type");
            let threshold: f64 = r.get("threshold");
            let question: String = r.get("question");
            let outcome_index: i64 = r.get("outcome_index");
            let label = format!(
                "{} | {} @ {:.2} (outcome {})",
                question, alert_type, threshold, outcome_index
            );
            (id, label)
        })
        .collect();

    let kb = alert_list_keyboard(&items);

    bot.send_message(msg.chat.id, "Select an alert to remove:")
        .reply_markup(kb)
        .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// /timezone
// ---------------------------------------------------------------------------

/// Update the user's timezone preference.
#[instrument(skip_all)]
pub async fn timezone(
    bot: Bot,
    msg: Message,
    tz_str: String,
    pool: SqlitePool,
    bot_id: Arc<String>,
) -> anyhow::Result<()> {
    let telegram_id = match msg.from.as_ref() {
        Some(u) => u.id.0 as i64,
        None => return Ok(()),
    };

    if tz_str.trim().is_empty() {
        bot.send_message(
            msg.chat.id,
            "Usage: /timezone America/New_York\n\
             Provide a valid IANA timezone name.",
        )
        .await?;
        return Ok(());
    }

    let tz_str = tz_str.trim().to_string();

    // Basic validation: check the string is non-empty and doesn't contain
    // clearly invalid characters.
    if tz_str.contains([';', '\'', '"', '\\']) {
        bot.send_message(msg.chat.id, "Invalid timezone string.").await?;
        return Ok(());
    }

    let user = get_or_create_user(&pool, telegram_id, &bot_id, None).await?;

    let now = chrono::Utc::now().naive_utc();
    sqlx::query("UPDATE users SET timezone = ?, updated_at = ? WHERE id = ?")
        .bind(&tz_str)
        .bind(now)
        .bind(user.id)
        .execute(&pool)
        .await?;

    bot.send_message(
        msg.chat.id,
        format!("Timezone updated to: {tz_str}"),
    )
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Callback-query: unsubscribe
// ---------------------------------------------------------------------------

/// Handle an `"unsub:{id}"` callback – deactivate a subscription.
#[instrument(skip_all)]
pub async fn callback_unsubscribe(
    bot: Bot,
    q: CallbackQuery,
    pool: SqlitePool,
    sub_id: i64,
) -> anyhow::Result<()> {
    bot.answer_callback_query(q.id.clone()).await?;

    let chat_id = match q.message.as_ref() {
        Some(m) => m.chat().id,
        None => return Ok(()),
    };

    let result = sqlx::query("UPDATE subscriptions SET is_active = 0 WHERE id = ?")
        .bind(sub_id)
        .execute(&pool)
        .await;

    match result {
        Ok(r) if r.rows_affected() > 0 => {
            bot.send_message(chat_id, "Subscription removed.").await?;
        }
        Ok(_) => {
            bot.send_message(chat_id, "Subscription not found.").await?;
        }
        Err(e) => {
            warn!(error=%e, sub_id, "failed to deactivate subscription");
            bot.send_message(chat_id, "Failed to remove subscription.").await?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Callback-query: remove alert
// ---------------------------------------------------------------------------

/// Handle an `"rm_alert:{id}"` callback – delete an alert.
#[instrument(skip_all)]
pub async fn callback_remove_alert(
    bot: Bot,
    q: CallbackQuery,
    pool: SqlitePool,
    alert_id: i64,
) -> anyhow::Result<()> {
    bot.answer_callback_query(q.id.clone()).await?;

    let result = sqlx::query("DELETE FROM alerts WHERE id = ?")
        .bind(alert_id)
        .execute(&pool)
        .await;

    let chat_id = match q.message.as_ref() {
        Some(m) => m.chat().id,
        None => return Ok(()),
    };

    match result {
        Ok(r) if r.rows_affected() > 0 => {
            bot.send_message(chat_id, "Alert removed.").await?;
        }
        Ok(_) => {
            bot.send_message(chat_id, "Alert not found.").await?;
        }
        Err(e) => {
            warn!(error=%e, alert_id, "failed to delete alert");
            bot.send_message(chat_id, "Failed to remove alert.").await?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// /subscribe and /alert  (thin wrappers that delegate to dialogues.rs)
// ---------------------------------------------------------------------------

/// Wrapper that starts the subscribe dialogue flow.
pub async fn subscribe(
    bot: Bot,
    dialogue: MyDialogue,
    msg: Message,
) -> anyhow::Result<()> {
    handle_subscribe_start(bot, dialogue, msg).await
}

/// Wrapper that starts the alert dialogue flow.
pub async fn alert(
    bot: Bot,
    dialogue: MyDialogue,
    msg: Message,
    pool: SqlitePool,
    bot_id: Arc<String>,
) -> anyhow::Result<()> {
    handle_alert_start(bot, dialogue, msg, pool, (*bot_id).clone()).await
}
