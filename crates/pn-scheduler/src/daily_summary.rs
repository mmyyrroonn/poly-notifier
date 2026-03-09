//! Daily summary scheduler job.
//!
//! [`DailySummaryJob`] runs on a configurable cron schedule (default: once per
//! day) and sends each user with active subscriptions a formatted price summary
//! via the notification channel.
//!
//! The job loops indefinitely, sleeping until the next scheduled instant, then
//! fetches all active user–subscription–market combinations and dispatches one
//! [`NotificationRequest`] per user.  It shuts down cleanly when the provided
//! [`CancellationToken`] is cancelled.
//!
//! # Example
//!
//! ```no_run
//! use tokio::sync::mpsc;
//! use tokio_util::sync::CancellationToken;
//! use sqlx::SqlitePool;
//! use pn_scheduler::DailySummaryJob;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let pool: SqlitePool = todo!("open database");
//!     let (tx, _rx) = mpsc::channel(64);
//!     let cancel = CancellationToken::new();
//!
//!     let job = DailySummaryJob::new(pool, tx, "0 0 9 * * *".to_string());
//!     job.run(cancel).await?;
//!     Ok(())
//! }
//! ```

use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use cron::Schedule;
use sqlx::SqlitePool;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use pn_common::events::{NotificationRequest, NotificationType};

// ---------------------------------------------------------------------------
// Row returned by the summary query
// ---------------------------------------------------------------------------

/// Flat row joining users, subscriptions, and markets, fetched in a single
/// query to avoid N+1 round trips.
#[derive(Debug, sqlx::FromRow)]
struct SummaryRow {
    user_id: i64,
    telegram_id: i64,
    bot_id: String,
    outcome_index: i32,
    question: String,
    /// JSON-encoded `Vec<String>` of outcome labels.
    outcomes: String,
    /// JSON-encoded `Vec<rust_decimal::Decimal>` of last prices.
    last_prices: String,
}

// ---------------------------------------------------------------------------
// DailySummaryJob
// ---------------------------------------------------------------------------

/// Sends a formatted daily portfolio summary to every user that has at least
/// one active subscription.
pub struct DailySummaryJob {
    pool: SqlitePool,
    notify_tx: mpsc::Sender<NotificationRequest>,
    cron_expr: String,
}

impl DailySummaryJob {
    /// Create a new job.
    ///
    /// # Arguments
    ///
    /// * `pool` – SQLite connection pool.
    /// * `notify_tx` – Channel to the notification dispatcher.
    /// * `cron_expr` – 6-field cron expression (sec min hr day month weekday),
    ///   e.g. `"0 0 9 * * *"` for 09:00 UTC every day.
    pub fn new(
        pool: SqlitePool,
        notify_tx: mpsc::Sender<NotificationRequest>,
        cron_expr: String,
    ) -> Self {
        Self {
            pool,
            notify_tx,
            cron_expr,
        }
    }

    /// Run the scheduler loop until `cancel` is cancelled.
    ///
    /// On each wake-up the job calls [`Self::send_summaries`]; any error is
    /// logged but does not abort the loop.
    pub async fn run(&self, cancel: CancellationToken) -> Result<()> {
        let schedule = Schedule::from_str(&self.cron_expr)
            .with_context(|| format!("invalid cron expression: {:?}", self.cron_expr))?;

        info!(cron = %self.cron_expr, "daily summary job started");

        loop {
            // Determine how long to sleep until the next scheduled instant.
            let delay = schedule
                .upcoming(Utc)
                .next()
                .and_then(|next| (next - Utc::now()).to_std().ok())
                .unwrap_or(Duration::from_secs(60));

            tracing::debug!(sleep_secs = delay.as_secs(), "sleeping until next summary run");

            tokio::select! {
                _ = tokio::time::sleep(delay) => {
                    info!("running daily summary");
                    if let Err(e) = self.send_summaries().await {
                        error!(error = %e, "daily summary failed");
                    }
                }
                _ = cancel.cancelled() => {
                    info!("daily summary job shutting down");
                    return Ok(());
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Fetch all active subscription rows, group them by user, format a
    /// message per user, and enqueue a [`NotificationRequest`] for each.
    async fn send_summaries(&self) -> Result<()> {
        let rows = self.fetch_summary_rows().await?;

        if rows.is_empty() {
            info!("no active subscriptions; skipping daily summary");
            return Ok(());
        }

        // Group rows by (user_id, telegram_id, bot_id).
        let mut user_map: HashMap<i64, (i64, String, Vec<SummaryRow>)> = HashMap::new();
        for row in rows {
            let entry = user_map
                .entry(row.user_id)
                .or_insert_with(|| (row.telegram_id, row.bot_id.clone(), Vec::new()));
            entry.2.push(row);
        }

        let now_str = Utc::now().format("%Y-%m-%d %H:%M UTC").to_string();
        let mut sent = 0u32;
        let mut failed = 0u32;

        for (user_id, (telegram_id, bot_id, subs)) in user_map {
            let message = match format_summary(&subs, &now_str) {
                Ok(msg) => msg,
                Err(e) => {
                    warn!(user_id, error = %e, "failed to format summary; skipping user");
                    failed += 1;
                    continue;
                }
            };

            let request = NotificationRequest {
                user_telegram_id: telegram_id,
                bot_id,
                message,
                notification_type: NotificationType::DailySummary,
            };

            if let Err(e) = self.notify_tx.send(request).await {
                error!(user_id, error = %e, "failed to enqueue daily summary notification");
                failed += 1;
            } else {
                sent += 1;
            }
        }

        info!(sent, failed, "daily summary dispatch complete");
        Ok(())
    }

    /// Query the database for every user that has at least one active
    /// subscription to an active market.
    async fn fetch_summary_rows(&self) -> Result<Vec<SummaryRow>> {
        let rows = sqlx::query_as::<_, SummaryRow>(
            "SELECT
                u.id          AS user_id,
                u.telegram_id,
                u.bot_id,
                s.outcome_index,
                m.question,
                m.outcomes,
                m.last_prices
            FROM users u
            JOIN subscriptions s ON s.user_id = u.id
            JOIN markets       m ON m.id      = s.market_id
            WHERE s.is_active = 1
              AND m.is_active = 1
            ORDER BY u.id, s.id",
        )
        .fetch_all(&self.pool)
        .await
        .context("failed to fetch summary rows from database")?;

        Ok(rows)
    }
}

// ---------------------------------------------------------------------------
// Message formatting
// ---------------------------------------------------------------------------

/// Format a multi-line summary message for a single user.
///
/// Returns an error if any row contains malformed JSON in `outcomes` or
/// `last_prices`.
fn format_summary(subs: &[SummaryRow], now_str: &str) -> Result<String> {
    let mut lines = Vec::with_capacity(subs.len() * 4 + 3);
    lines.push("📊 Daily Summary".to_string());
    lines.push(String::new());

    for (idx, row) in subs.iter().enumerate() {
        let outcomes: Vec<String> = serde_json::from_str(&row.outcomes)
            .with_context(|| format!("malformed outcomes JSON for question {:?}", row.question))?;

        let last_prices: Vec<f64> = serde_json::from_str(&row.last_prices)
            .with_context(|| {
                format!("malformed last_prices JSON for question {:?}", row.question)
            })?;

        // Determine the label and price for the subscribed outcome.
        let outcome_label = outcomes
            .get(row.outcome_index as usize)
            .cloned()
            .unwrap_or_else(|| format!("Outcome {}", row.outcome_index));

        // Build the price line for every outcome of this market so the user
        // can see the full picture.
        let price_parts: Vec<String> = outcomes
            .iter()
            .zip(last_prices.iter().chain(std::iter::repeat(&0.0_f64)))
            .map(|(label, &price)| format!("{}: ${:.2}", label, price))
            .collect();

        lines.push(format!("{}. {}", idx + 1, row.question));

        // Highlight the subscribed outcome.
        let subscribed_price = last_prices
            .get(row.outcome_index as usize)
            .copied()
            .unwrap_or(0.0);
        lines.push(format!(
            "   Tracking: {} @ ${:.2}",
            outcome_label, subscribed_price
        ));
        lines.push(format!("   {}", price_parts.join(" | ")));
        lines.push(String::new());
    }

    lines.push(format!("Updated: {}", now_str));

    Ok(lines.join("\n"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_row(question: &str, outcome_index: i32, outcomes: &[&str], prices: &[f64]) -> SummaryRow {
        SummaryRow {
            user_id: 1,
            telegram_id: 111,
            bot_id: "bot".to_string(),
            outcome_index,
            question: question.to_string(),
            outcomes: serde_json::to_string(outcomes).unwrap(),
            last_prices: serde_json::to_string(prices).unwrap(),
        }
    }

    #[test]
    fn format_summary_binary_market() {
        let rows = vec![make_row("Will X happen?", 0, &["Yes", "No"], &[0.55, 0.45])];
        let msg = format_summary(&rows, "2024-01-15 09:00 UTC").unwrap();
        assert!(msg.contains("Will X happen?"));
        assert!(msg.contains("Yes: $0.55"));
        assert!(msg.contains("No: $0.45"));
        assert!(msg.contains("Updated: 2024-01-15 09:00 UTC"));
    }

    #[test]
    fn format_summary_multiple_markets() {
        let rows = vec![
            make_row("Will X happen?", 0, &["Yes", "No"], &[0.55, 0.45]),
            make_row("Will Y happen?", 0, &["Yes", "No"], &[0.72, 0.28]),
        ];
        let msg = format_summary(&rows, "2024-01-15 09:00 UTC").unwrap();
        assert!(msg.contains("1. Will X happen?"));
        assert!(msg.contains("2. Will Y happen?"));
    }

    #[test]
    fn format_summary_empty_prices_uses_zero() {
        let rows = vec![make_row("Will Z happen?", 0, &["Yes", "No"], &[])];
        let msg = format_summary(&rows, "2024-01-15 09:00 UTC").unwrap();
        // Should not panic; prices default to $0.00
        assert!(msg.contains("Will Z happen?"));
    }

    #[test]
    fn format_summary_malformed_outcomes_returns_error() {
        let row = SummaryRow {
            user_id: 1,
            telegram_id: 1,
            bot_id: "bot".to_string(),
            outcome_index: 0,
            question: "Q?".to_string(),
            outcomes: "not-json".to_string(),
            last_prices: "[]".to_string(),
        };
        assert!(format_summary(&[row], "now").is_err());
    }
}
