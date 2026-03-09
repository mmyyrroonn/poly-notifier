//! Command handler implementations.
//!
//! Each function corresponds to one [`Command`] variant and is called by the
//! teloxide dispatcher after the command text has been parsed.

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
    keyboards::unsubscribe_keyboard,
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
            .map(|p| format!("{:.0}%", *p * 100.0))
            .unwrap_or_else(|| "N/A".to_string());

        // Show alerts for this subscription.
        let alert_rows = sqlx::query(
            "SELECT alert_type, threshold FROM alerts WHERE subscription_id = ? ORDER BY created_at",
        )
        .bind(s.subscription_id)
        .fetch_all(&pool)
        .await
        .unwrap_or_default();

        let alert_str = if alert_rows.is_empty() {
            "   No alerts".to_string()
        } else {
            alert_rows
                .iter()
                .map(|r| {
                    let at: String = r.get("alert_type");
                    let th: f64 = r.get("threshold");
                    format!("   Alert: {} {:.0}%", at, th * 100.0)
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        lines.push(format!("{}. {}", i + 1, s.question));
        lines.push(format!("   {} @ {}", outcome_label, price_str));
        lines.push(alert_str);
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
                .map(|p| {
                    use rust_decimal::Decimal;
                    let pct = *p * Decimal::from(100);
                    format!("{pct:.0}%")
                })
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
/// Alerts are cascade-deleted by the DB when the subscription is removed.
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

    // Delete the subscription row so CASCADE removes associated alerts.
    let result = sqlx::query("DELETE FROM subscriptions WHERE id = ?")
        .bind(sub_id)
        .execute(&pool)
        .await;

    match result {
        Ok(r) if r.rows_affected() > 0 => {
            bot.send_message(chat_id, "Subscription and its alerts removed.")
                .await?;
        }
        Ok(_) => {
            bot.send_message(chat_id, "Subscription not found.").await?;
        }
        Err(e) => {
            warn!(error=%e, sub_id, "failed to delete subscription");
            bot.send_message(chat_id, "Failed to remove subscription.").await?;
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
