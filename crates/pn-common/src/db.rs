//! Database connection pool and common query helpers.
//!
//! All functions return [`crate::error::Result`] so callers can propagate
//! errors through their own `?` chains.
//!
//! ## Compile-time note
//!
//! Query helpers use the runtime `sqlx::query_as` / `sqlx::query` forms
//! (rather than the `query_as!` / `query!` macros) so that the crate can be
//! built without a live database connection or an `.sqlx` offline cache.
//! Type safety is preserved through the `sqlx::FromRow` derive on every
//! model struct.

use chrono::Utc;
use sqlx::{sqlite::SqlitePoolOptions, SqlitePool};
use tracing::info;

use crate::{
    error::{Error, Result},
    models::{Alert, Feedback, Market, NotificationLog, Subscription, SubscriptionDetail, User},
};

// ---------------------------------------------------------------------------
// Pool initialisation
// ---------------------------------------------------------------------------

/// Create a [`SqlitePool`] and run all pending migrations.
///
/// The migrations directory is embedded at compile time via
/// `sqlx::migrate!("../../migrations")`, which resolves to
/// `<workspace-root>/migrations/`.
///
/// # Arguments
///
/// * `database_url` - SQLx connection URL, e.g. `sqlite:poly-notifier.db`.
/// * `max_connections` - Size of the connection pool.
///
/// # Example
///
/// ```no_run
/// use pn_common::db::init_db;
///
/// #[tokio::main]
/// async fn main() {
///     let pool = init_db("sqlite:poly-notifier.db", 5)
///         .await
///         .expect("failed to initialise database");
/// }
/// ```
pub async fn init_db(database_url: &str, max_connections: u32) -> Result<SqlitePool> {
    info!(%database_url, max_connections, "initialising SQLite pool");

    let pool = SqlitePoolOptions::new()
        .max_connections(max_connections)
        // Enable WAL mode and foreign-key enforcement for every connection.
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                use sqlx::Executor;
                conn.execute("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
                    .await
                    .map(|_| ())
            })
        })
        .connect(database_url)
        .await?;

    info!("running database migrations");
    sqlx::migrate!("../../migrations").run(&pool).await?;
    info!("migrations complete");

    Ok(pool)
}

// ---------------------------------------------------------------------------
// User helpers
// ---------------------------------------------------------------------------

/// Return the [`User`] row for a given `(telegram_id, bot_id)` pair,
/// creating one with defaults if it does not yet exist.
///
/// The returned value always reflects the current persisted state (including
/// any columns that were already set before this call).
pub async fn get_or_create_user(
    pool: &SqlitePool,
    telegram_id: i64,
    bot_id: &str,
    username: Option<&str>,
) -> Result<User> {
    // Try to find an existing row first.
    let existing: Option<User> = sqlx::query_as(
        "SELECT id, telegram_id, bot_id, username, tier, max_subscriptions, \
         timezone, created_at, updated_at \
         FROM users \
         WHERE telegram_id = ? AND bot_id = ?",
    )
    .bind(telegram_id)
    .bind(bot_id)
    .fetch_optional(pool)
    .await?;

    if let Some(user) = existing {
        // Opportunistically update the username if it has changed.
        if username.is_some() && username != user.username.as_deref() {
            let now = Utc::now().naive_utc();
            sqlx::query(
                "UPDATE users SET username = ?, updated_at = ? WHERE id = ?",
            )
            .bind(username)
            .bind(now)
            .bind(user.id)
            .execute(pool)
            .await?;

            return fetch_user_by_id(pool, user.id).await;
        }
        return Ok(user);
    }

    // Insert a new user with all defaults.
    let inserted_id: i64 = sqlx::query(
        "INSERT INTO users (telegram_id, bot_id, username) VALUES (?, ?, ?)",
    )
    .bind(telegram_id)
    .bind(bot_id)
    .bind(username)
    .execute(pool)
    .await?
    .last_insert_rowid();

    fetch_user_by_id(pool, inserted_id).await
}

/// Internal: fetch a single user row by primary key.
async fn fetch_user_by_id(pool: &SqlitePool, id: i64) -> Result<User> {
    sqlx::query_as(
        "SELECT id, telegram_id, bot_id, username, tier, max_subscriptions, \
         timezone, created_at, updated_at \
         FROM users \
         WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .map_err(Error::Database)
}

/// Fetch all [`User`] rows ordered by primary key.
pub async fn get_all_users(pool: &SqlitePool) -> Result<Vec<User>> {
    sqlx::query_as(
        "SELECT id, telegram_id, bot_id, username, tier, max_subscriptions, \
         timezone, created_at, updated_at \
         FROM users \
         ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .map_err(Error::Database)
}

// ---------------------------------------------------------------------------
// Subscription helpers
// ---------------------------------------------------------------------------

/// Return all active subscriptions for a specific user, joined with market
/// metadata needed to display them in the bot UI.
///
/// Results are ordered by subscription creation time.
pub async fn get_user_subscriptions(
    pool: &SqlitePool,
    user_id: i64,
) -> Result<Vec<SubscriptionDetail>> {
    sqlx::query_as(
        "SELECT \
             s.id          AS subscription_id, \
             s.outcome_index, \
             m.id          AS market_id, \
             m.condition_id, \
             m.question, \
             m.token_ids, \
             m.last_prices, \
             u.id          AS user_id, \
             u.telegram_id, \
             u.bot_id, \
             u.timezone \
         FROM subscriptions s \
         JOIN markets  m ON m.id = s.market_id \
         JOIN users    u ON u.id = s.user_id \
         WHERE s.user_id = ? \
           AND s.is_active = 1 \
           AND m.is_active = 1 \
         ORDER BY s.created_at",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .map_err(Error::Database)
}

/// Return every active subscription in the system together with its market and
/// user details.  Used by the alert engine to build its in-memory evaluation
/// table on startup or after a refresh.
pub async fn get_active_subscriptions_with_details(
    pool: &SqlitePool,
) -> Result<Vec<SubscriptionDetail>> {
    sqlx::query_as(
        "SELECT \
             s.id          AS subscription_id, \
             s.outcome_index, \
             m.id          AS market_id, \
             m.condition_id, \
             m.question, \
             m.token_ids, \
             m.last_prices, \
             u.id          AS user_id, \
             u.telegram_id, \
             u.bot_id, \
             u.timezone \
         FROM subscriptions s \
         JOIN markets  m ON m.id = s.market_id \
         JOIN users    u ON u.id = s.user_id \
         WHERE s.is_active = 1 \
           AND m.is_active = 1 \
         ORDER BY s.id",
    )
    .fetch_all(pool)
    .await
    .map_err(Error::Database)
}

/// Return the raw [`Subscription`] rows for a user (active only).
pub async fn get_subscriptions_for_user(
    pool: &SqlitePool,
    user_id: i64,
) -> Result<Vec<Subscription>> {
    sqlx::query_as(
        "SELECT id, user_id, market_id, outcome_index, is_active, created_at \
         FROM subscriptions \
         WHERE user_id = ? AND is_active = 1 \
         ORDER BY created_at",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .map_err(Error::Database)
}

/// Count the number of active subscriptions a user currently has.
pub async fn count_active_subscriptions(pool: &SqlitePool, user_id: i64) -> Result<i64> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM subscriptions WHERE user_id = ? AND is_active = 1",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

// ---------------------------------------------------------------------------
// Market helpers
// ---------------------------------------------------------------------------

/// Upsert a market row by `condition_id`.  Returns the row `id`.
///
/// If the market already exists the `question`, `slug`, `outcomes`,
/// `token_ids`, `is_active`, and `updated_at` columns are refreshed.
pub async fn upsert_market(
    pool: &SqlitePool,
    condition_id: &str,
    question: &str,
    slug: Option<&str>,
    outcomes_json: &str,
    token_ids_json: &str,
) -> Result<i64> {
    let now = Utc::now().naive_utc();

    // SQLite's RETURNING clause returns the row id whether it was inserted or
    // updated.
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO markets (condition_id, question, slug, outcomes, token_ids, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?) \
         ON CONFLICT(condition_id) DO UPDATE SET \
             question   = excluded.question, \
             slug       = excluded.slug, \
             outcomes   = excluded.outcomes, \
             token_ids  = excluded.token_ids, \
             is_active  = 1, \
             updated_at = excluded.updated_at \
         RETURNING id",
    )
    .bind(condition_id)
    .bind(question)
    .bind(slug)
    .bind(outcomes_json)
    .bind(token_ids_json)
    .bind(now)
    .fetch_one(pool)
    .await?;

    Ok(row.0)
}

/// Look up a market by its `condition_id`.
pub async fn get_market_by_condition_id(
    pool: &SqlitePool,
    condition_id: &str,
) -> Result<Option<Market>> {
    sqlx::query_as(
        "SELECT id, condition_id, question, slug, outcomes, token_ids, last_prices, \
         is_active, created_at, updated_at \
         FROM markets \
         WHERE condition_id = ?",
    )
    .bind(condition_id)
    .fetch_optional(pool)
    .await
    .map_err(Error::Database)
}

/// Update the `last_prices` JSON column for a market.
pub async fn update_market_prices(
    pool: &SqlitePool,
    market_id: i64,
    last_prices_json: &str,
) -> Result<()> {
    let now = Utc::now().naive_utc();
    sqlx::query(
        "UPDATE markets SET last_prices = ?, updated_at = ? WHERE id = ?",
    )
    .bind(last_prices_json)
    .bind(now)
    .bind(market_id)
    .execute(pool)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Alert helpers
// ---------------------------------------------------------------------------

/// Fetch all alerts belonging to a specific subscription.
pub async fn get_alerts_for_subscription(
    pool: &SqlitePool,
    subscription_id: i64,
) -> Result<Vec<Alert>> {
    sqlx::query_as(
        "SELECT id, subscription_id, alert_type, threshold, is_triggered, \
         cooldown_minutes, last_triggered_at, created_at \
         FROM alerts \
         WHERE subscription_id = ? \
         ORDER BY created_at",
    )
    .bind(subscription_id)
    .fetch_all(pool)
    .await
    .map_err(Error::Database)
}

/// Mark an alert as triggered and record the current timestamp.
pub async fn record_alert_triggered(pool: &SqlitePool, alert_id: i64) -> Result<()> {
    let now = Utc::now().naive_utc();
    sqlx::query(
        "UPDATE alerts SET is_triggered = 1, last_triggered_at = ? WHERE id = ?",
    )
    .bind(now)
    .bind(alert_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Reset the `is_triggered` flag for an alert (e.g. after cooldown expires).
pub async fn reset_alert_triggered(pool: &SqlitePool, alert_id: i64) -> Result<()> {
    sqlx::query("UPDATE alerts SET is_triggered = 0 WHERE id = ?")
        .bind(alert_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Notification log helpers
// ---------------------------------------------------------------------------

/// Insert a record into `notification_log` and return its `id`.
pub async fn insert_notification_log(
    pool: &SqlitePool,
    user_id: i64,
    alert_id: Option<i64>,
    notification_type: &str,
    message: &str,
    delivered: bool,
    error_message: Option<&str>,
) -> Result<i64> {
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO notification_log \
             (user_id, alert_id, notification_type, message, delivered, error_message) \
         VALUES (?, ?, ?, ?, ?, ?) \
         RETURNING id",
    )
    .bind(user_id)
    .bind(alert_id)
    .bind(notification_type)
    .bind(message)
    .bind(delivered)
    .bind(error_message)
    .fetch_one(pool)
    .await?;

    Ok(row.0)
}

/// Mark a previously-inserted notification as delivered.
pub async fn mark_notification_delivered(pool: &SqlitePool, log_id: i64) -> Result<()> {
    sqlx::query("UPDATE notification_log SET delivered = 1 WHERE id = ?")
        .bind(log_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Return the internal `users.id` for a given Telegram user ID, or `None` if
/// no matching row exists.
///
/// Used by the notification dispatcher to resolve the FK needed by the
/// `notification_log` table without loading the full [`User`] struct.
pub async fn resolve_user_id_by_telegram(
    pool: &SqlitePool,
    telegram_id: i64,
) -> Result<Option<i64>> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT id FROM users WHERE telegram_id = ? LIMIT 1",
    )
    .bind(telegram_id)
    .fetch_optional(pool)
    .await
    .map_err(Error::Database)?;

    Ok(row.map(|(id,)| id))
}

// ---------------------------------------------------------------------------
// Feedback helpers
// ---------------------------------------------------------------------------

/// Insert a feedback message and return its `id`.
pub async fn insert_feedback(
    pool: &SqlitePool,
    user_id: i64,
    message: &str,
) -> Result<i64> {
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO feedback (user_id, message) VALUES (?, ?) RETURNING id",
    )
    .bind(user_id)
    .bind(message)
    .fetch_one(pool)
    .await?;

    Ok(row.0)
}

/// Return the most recent feedback timestamp for a user, or `None` if they
/// have never submitted feedback.
pub async fn get_last_feedback_time(
    pool: &SqlitePool,
    user_id: i64,
) -> Result<Option<chrono::NaiveDateTime>> {
    let row: Option<(chrono::NaiveDateTime,)> = sqlx::query_as(
        "SELECT created_at FROM feedback WHERE user_id = ? ORDER BY created_at DESC LIMIT 1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|(ts,)| ts))
}

/// Fetch all feedback entries, newest first.
pub async fn get_all_feedback(pool: &SqlitePool) -> Result<Vec<Feedback>> {
    sqlx::query_as(
        "SELECT id, user_id, message, created_at \
         FROM feedback \
         ORDER BY created_at DESC",
    )
    .fetch_all(pool)
    .await
    .map_err(Error::Database)
}

// ---------------------------------------------------------------------------
// Notification log helpers (continued)
// ---------------------------------------------------------------------------

/// Fetch recent notification log entries for a user, newest first.
pub async fn get_recent_notifications(
    pool: &SqlitePool,
    user_id: i64,
    limit: i64,
) -> Result<Vec<NotificationLog>> {
    sqlx::query_as(
        "SELECT id, user_id, alert_id, notification_type, message, delivered, \
         error_message, created_at \
         FROM notification_log \
         WHERE user_id = ? \
         ORDER BY created_at DESC \
         LIMIT ?",
    )
    .bind(user_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(Error::Database)
}
