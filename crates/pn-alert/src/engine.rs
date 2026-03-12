//! Main alert engine service.
//!
//! [`AlertEngine`] subscribes to the broadcast [`PriceUpdate`] channel,
//! evaluates every active [`AlertRule`] for the updated token, and forwards
//! [`NotificationRequest`]s to the notification dispatcher when rules fire.
//!
//! The engine keeps an in-memory cache of alert rules keyed by `token_id` and
//! refreshes it from the database on a configurable interval so that newly
//! created or deleted alerts are picked up without a restart.

use std::collections::HashMap;

use anyhow::Context;
use chrono::Utc;
use dashmap::DashMap;
use rust_decimal::Decimal;
use sqlx::SqlitePool;
use tokio::sync::{broadcast, mpsc};
use tokio::time::{self, Duration};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use pn_common::{
    config::AlertConfig,
    events::{NotificationRequest, NotificationType, PriceUpdate},
    models::AlertType,
};

use crate::{dedup::AlertDedup, rules::AlertRule};

// ---------------------------------------------------------------------------
// Raw query result
// ---------------------------------------------------------------------------

/// Intermediate row type returned by the cache-refresh SQL query.
///
/// Fields mirror the JOIN output exactly so `sqlx` can map them with
/// `query_as!`.  The `token_ids` JSON blob is decoded later to extract the
/// specific token relevant to the subscription's `outcome_index`.
#[derive(sqlx::FromRow)]
struct AlertRow {
    alert_id: i64,
    subscription_id: i64,
    alert_type: String,
    /// Stored as REAL in SQLite; we convert to Decimal after fetch.
    threshold: f64,
    cooldown_minutes: i32,
    last_triggered_at: Option<chrono::NaiveDateTime>,
    user_telegram_id: i64,
    bot_id: String,
    outcome_index: i32,
    market_question: String,
    /// Raw JSON array string, e.g. `["0xabc","0xdef"]`.
    token_ids: String,
}

// ---------------------------------------------------------------------------
// AlertEngine
// ---------------------------------------------------------------------------

/// Configuration for the alert engine.
///
/// Typically sourced from [`pn_common::config::AlertConfig`] and passed at
/// construction time.
pub struct EngineConfig {
    /// How often the in-memory alert cache is refreshed from the database.
    pub cache_refresh_interval_secs: u64,
    /// Fallback cooldown used when an alert row has `cooldown_minutes = 0`.
    pub default_cooldown_minutes: i64,
    /// How often (seconds) in-memory prices are flushed to `markets.last_prices`.
    pub price_flush_interval_secs: u64,
}

impl From<AlertConfig> for EngineConfig {
    fn from(cfg: AlertConfig) -> Self {
        Self {
            cache_refresh_interval_secs: cfg.cache_refresh_interval_secs,
            default_cooldown_minutes: cfg.default_cooldown_minutes,
            price_flush_interval_secs: cfg.price_flush_interval_secs,
        }
    }
}

/// The alert engine: watches price ticks and fires notifications.
///
/// Construct with [`AlertEngine::new`] and drive with [`AlertEngine::run`].
///
/// # Shutdown
///
/// Pass a [`CancellationToken`] to [`AlertEngine::run`].  When the token is
/// cancelled the engine drains any in-flight work and returns.
pub struct AlertEngine {
    pool: SqlitePool,
    price_rx: broadcast::Receiver<PriceUpdate>,
    notify_tx: mpsc::Sender<NotificationRequest>,
    config: EngineConfig,
    /// `token_id` → list of rules watching that token.
    rule_cache: DashMap<String, Vec<AlertRule>>,
    /// `token_id` → last observed price (for cross-detection).
    prev_prices: DashMap<String, Decimal>,
    dedup: AlertDedup,
    /// `condition_id` → (market_id, ordered list of token_ids).
    /// Used to map price updates back to the correct market and outcome index
    /// so that `markets.last_prices` can be kept in sync.
    condition_tokens: DashMap<String, (i64, Vec<String>)>,
}

impl AlertEngine {
    /// Creates a new engine from an [`AlertConfig`].
    ///
    /// `price_rx` should be obtained by calling
    /// `broadcast::Sender::subscribe()` on the monitor's sender.
    pub fn new(
        pool: SqlitePool,
        price_rx: broadcast::Receiver<PriceUpdate>,
        notify_tx: mpsc::Sender<NotificationRequest>,
        config: AlertConfig,
    ) -> Self {
        Self {
            pool,
            price_rx,
            notify_tx,
            config: EngineConfig::from(config),
            rule_cache: DashMap::new(),
            prev_prices: DashMap::new(),
            dedup: AlertDedup::new(),
            condition_tokens: DashMap::new(),
        }
    }

    /// Creates a new engine with a pre-built [`EngineConfig`].
    ///
    /// Useful in tests where you want fine-grained control over the config
    /// without going through the full [`AlertConfig`] deserialisation path.
    pub fn with_config(
        pool: SqlitePool,
        price_rx: broadcast::Receiver<PriceUpdate>,
        notify_tx: mpsc::Sender<NotificationRequest>,
        config: EngineConfig,
    ) -> Self {
        Self {
            pool,
            price_rx,
            notify_tx,
            config,
            rule_cache: DashMap::new(),
            prev_prices: DashMap::new(),
            dedup: AlertDedup::new(),
            condition_tokens: DashMap::new(),
        }
    }

    /// Runs the engine until `cancel_token` is cancelled.
    ///
    /// The method performs an initial cache refresh before entering the main
    /// event loop, so it is ready to process price ticks immediately.
    pub async fn run(mut self, cancel_token: CancellationToken) -> anyhow::Result<()> {
        // Perform an initial load so the engine is not empty on the first tick.
        if let Err(e) = self.refresh_cache().await {
            error!("initial alert cache refresh failed: {e:#}");
        }

        let refresh_interval = Duration::from_secs(self.config.cache_refresh_interval_secs);
        let mut cache_ticker = time::interval(refresh_interval);
        // Skip the first immediate tick – we already refreshed above.
        cache_ticker.tick().await;

        let flush_interval = Duration::from_secs(self.config.price_flush_interval_secs);
        let mut flush_ticker = time::interval(flush_interval);
        flush_ticker.tick().await;

        info!("alert engine started");

        loop {
            tokio::select! {
                biased;

                // Graceful shutdown takes highest priority.
                _ = cancel_token.cancelled() => {
                    info!("alert engine shutting down");
                    // Final flush before exit.
                    if let Err(e) = self.flush_prices_to_db().await {
                        error!("final price flush failed: {e:#}");
                    }
                    break;
                }

                // Periodic cache refresh.
                _ = cache_ticker.tick() => {
                    if let Err(e) = self.refresh_cache().await {
                        error!("alert cache refresh failed: {e:#}");
                    }
                }

                // Periodic price flush to DB.
                _ = flush_ticker.tick() => {
                    if let Err(e) = self.flush_prices_to_db().await {
                        error!("price flush to DB failed: {e:#}");
                    }
                }

                // Price tick from the monitor.
                result = self.price_rx.recv() => {
                    match result {
                        Ok(update) => {
                            if let Err(e) = self.handle_price_update(update).await {
                                error!("error handling price update: {e:#}");
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!("alert engine lagged, skipped {n} price updates");
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            info!("price broadcast channel closed; alert engine exiting");
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Fetches all active alert rules from the database and rebuilds the
    /// in-memory cache.
    async fn refresh_cache(&self) -> anyhow::Result<()> {
        let rows: Vec<AlertRow> = sqlx::query_as(
            r#"
            SELECT
                a.id            AS alert_id,
                a.subscription_id,
                a.alert_type,
                a.threshold,
                a.cooldown_minutes,
                a.last_triggered_at,
                u.telegram_id   AS user_telegram_id,
                u.bot_id,
                s.outcome_index,
                m.question      AS market_question,
                m.token_ids
            FROM alerts a
            JOIN subscriptions s ON s.id = a.subscription_id
            JOIN markets       m ON m.id = s.market_id
            JOIN users         u ON u.id = s.user_id
            WHERE s.is_active = 1
              AND m.is_active = 1
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .context("fetching alert rows from DB")?;

        // Build a temporary map so we replace the cache atomically per-token.
        let mut by_token: HashMap<String, Vec<AlertRule>> = HashMap::new();
        let mut all_rules: Vec<AlertRule> = Vec::with_capacity(rows.len());

        for row in rows {
            // Parse the JSON token_ids array.
            let token_ids: Vec<String> = match serde_json::from_str(&row.token_ids) {
                Ok(ids) => ids,
                Err(e) => {
                    warn!(
                        alert_id = row.alert_id,
                        "invalid token_ids JSON '{}': {e}", row.token_ids
                    );
                    continue;
                }
            };

            let idx = row.outcome_index as usize;
            let token_id = match token_ids.get(idx) {
                Some(t) => t.clone(),
                None => {
                    warn!(
                        alert_id = row.alert_id,
                        outcome_index = row.outcome_index,
                        token_ids = row.token_ids,
                        "outcome_index out of bounds"
                    );
                    continue;
                }
            };

            let alert_type = match row.alert_type.parse::<AlertType>() {
                Ok(t) => t,
                Err(e) => {
                    warn!(alert_id = row.alert_id, "unknown alert_type: {e}");
                    continue;
                }
            };

            let threshold = Decimal::try_from(row.threshold).unwrap_or_else(|_| {
                warn!(alert_id = row.alert_id, "threshold conversion failed");
                Decimal::ZERO
            });

            let rule = AlertRule {
                alert_id: row.alert_id,
                subscription_id: row.subscription_id,
                user_telegram_id: row.user_telegram_id,
                bot_id: row.bot_id,
                token_id: token_id.clone(),
                outcome_index: row.outcome_index,
                market_question: row.market_question,
                alert_type,
                threshold,
                cooldown_minutes: row.cooldown_minutes,
                last_triggered_at: row.last_triggered_at,
            };

            by_token
                .entry(token_id)
                .or_default()
                .push(rule.clone());
            all_rules.push(rule);
        }

        // Replace all token entries in the shared cache.
        self.rule_cache.clear();
        for (token_id, rules) in by_token {
            self.rule_cache.insert(token_id, rules);
        }

        // Seed the dedup store with DB-persisted last_triggered_at values.
        self.dedup.load_from_alerts(&all_rules);

        debug!(rule_count = all_rules.len(), "alert cache refreshed");

        // Also refresh the condition → (market_id, token_ids) mapping used for
        // price persistence.
        if let Err(e) = self.refresh_condition_tokens().await {
            warn!("failed to refresh condition_tokens map: {e:#}");
        }

        Ok(())
    }

    /// Refresh the `condition_tokens` mapping from the database.
    ///
    /// Loads all active markets that have at least one active subscription so
    /// that incoming price updates can be mapped back to the correct market row
    /// and outcome index.
    async fn refresh_condition_tokens(&self) -> anyhow::Result<()> {
        let rows: Vec<(i64, String, String)> = sqlx::query_as(
            "SELECT DISTINCT m.id, m.condition_id, m.token_ids \
             FROM markets m \
             INNER JOIN subscriptions s ON s.market_id = m.id \
             WHERE m.is_active = 1 AND s.is_active = 1",
        )
        .fetch_all(&self.pool)
        .await
        .context("fetching condition_tokens from DB")?;

        self.condition_tokens.clear();
        for (market_id, condition_id, token_ids_json) in rows {
            let token_ids: Vec<String> = match serde_json::from_str(&token_ids_json) {
                Ok(ids) => ids,
                Err(e) => {
                    warn!(
                        condition_id = %condition_id,
                        "invalid token_ids JSON: {e}"
                    );
                    continue;
                }
            };
            self.condition_tokens
                .insert(condition_id, (market_id, token_ids));
        }

        debug!(
            count = self.condition_tokens.len(),
            "condition_tokens map refreshed"
        );
        Ok(())
    }

    /// Flush all in-memory prices to the database.
    ///
    /// For each condition in `condition_tokens`, build the `last_prices` JSON
    /// array from `prev_prices` and write it to the `markets` table.
    async fn flush_prices_to_db(&self) -> anyhow::Result<()> {
        let mut flushed = 0u32;

        for entry in self.condition_tokens.iter() {
            let condition_id = entry.key();
            let (market_id, token_ids) = entry.value();

            // Build the prices array by looking up each token's latest price.
            let prices: Vec<Decimal> = token_ids
                .iter()
                .map(|tid| {
                    self.prev_prices
                        .get(tid)
                        .map(|p| *p)
                        .unwrap_or(Decimal::ZERO)
                })
                .collect();

            // Skip if all prices are zero (no updates received yet).
            if prices.iter().all(|p| p.is_zero()) {
                continue;
            }

            let prices_json = serde_json::to_string(&prices).unwrap_or_else(|_| "[]".to_string());

            if let Err(e) =
                pn_common::db::update_market_prices(&self.pool, *market_id, &prices_json).await
            {
                warn!(
                    condition_id = %condition_id,
                    market_id = market_id,
                    "failed to flush prices: {e}"
                );
            } else {
                flushed += 1;
            }
        }

        if flushed > 0 {
            debug!(flushed, "prices flushed to DB");
        }
        Ok(())
    }

    /// Evaluates all rules for the updated token and fires notifications for
    /// those that pass evaluation and are not in cooldown.
    async fn handle_price_update(&self, update: PriceUpdate) -> anyhow::Result<()> {
        let current_price = update.price;
        let previous_price = self
            .prev_prices
            .get(&update.token_id)
            .map(|p| *p);

        // Store current price as previous for the next tick before evaluating,
        // so a crash mid-evaluation does not lose the price history.
        self.prev_prices
            .insert(update.token_id.clone(), current_price);

        let rules_ref = match self.rule_cache.get(&update.token_id) {
            Some(r) => r,
            None => return Ok(()), // No alerts watching this token
        };

        for rule in rules_ref.iter() {
            if !rule.evaluate(current_price, previous_price) {
                continue;
            }

            let cooldown = if rule.cooldown_minutes > 0 {
                rule.cooldown_minutes
            } else {
                self.config.default_cooldown_minutes as i32
            };

            if !self.dedup.can_fire(rule.alert_id, cooldown) {
                debug!(
                    alert_id = rule.alert_id,
                    "alert suppressed by cooldown"
                );
                continue;
            }

            // Mark as fired in the dedup store immediately to prevent a
            // parallel tick from double-firing while the DB write is in flight.
            self.dedup.mark_fired(rule.alert_id);

            let message = format_notification(rule, current_price, &update);

            let notification = NotificationRequest {
                user_telegram_id: rule.user_telegram_id,
                bot_id: rule.bot_id.clone(),
                message,
                notification_type: NotificationType::Alert,
            };

            if let Err(e) = self.notify_tx.send(notification).await {
                error!(
                    alert_id = rule.alert_id,
                    "notification channel closed: {e}"
                );
            }

            // Persist the trigger to the database.
            if let Err(e) = self.persist_triggered(rule.alert_id).await {
                error!(
                    alert_id = rule.alert_id,
                    "failed to persist alert trigger: {e:#}"
                );
            }

            info!(
                alert_id = rule.alert_id,
                token_id = %update.token_id,
                price = %current_price,
                "alert fired"
            );
        }

        Ok(())
    }

    /// Updates the database row for a fired alert.
    async fn persist_triggered(&self, alert_id: i64) -> anyhow::Result<()> {
        let now = Utc::now().naive_utc();
        sqlx::query(
            r#"
            UPDATE alerts
            SET is_triggered     = 1,
                last_triggered_at = ?
            WHERE id = ?
            "#,
        )
        .bind(now)
        .bind(alert_id)
        .execute(&self.pool)
        .await
        .context("updating alert trigger in DB")?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Notification formatting
// ---------------------------------------------------------------------------

/// Formats the Telegram HTML notification message for a triggered alert.
fn format_notification(rule: &AlertRule, price: Decimal, update: &PriceUpdate) -> String {
    let alert_type_label = match rule.alert_type {
        AlertType::Above => "Above",
        AlertType::Below => "Below",
        AlertType::Cross => "Cross",
    };

    let timestamp = update.timestamp.format("%Y-%m-%d %H:%M:%S UTC");

    let threshold_pct = rule.threshold * Decimal::from(100);
    format!(
        "🔔 <b>Price Alert</b>\n\n\
         📊 {question}\n\
         📈 Type: {alert_type} {threshold}%\n\
         💰 Current: {price}\n\
         ⏰ {timestamp}",
        question = rule.market_question,
        alert_type = alert_type_label,
        threshold = threshold_pct,
        price = price,
        timestamp = timestamp,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pn_common::models::AlertType;
    use rust_decimal::dec;

    fn make_rule(alert_type: AlertType) -> AlertRule {
        AlertRule {
            alert_id: 1,
            subscription_id: 1,
            user_telegram_id: 123,
            bot_id: "bot".to_string(),
            token_id: "0xabc".to_string(),
            outcome_index: 0,
            market_question: "Will X happen?".to_string(),
            alert_type,
            threshold: dec!(0.70),
            cooldown_minutes: 60,
            last_triggered_at: None,
        }
    }

    #[test]
    fn notification_contains_required_fields() {
        let rule = make_rule(AlertType::Above);
        let update = PriceUpdate {
            token_id: "0xabc".to_string(),
            condition_id: "0xdef".to_string(),
            price: dec!(0.75),
            timestamp: chrono::DateTime::parse_from_rfc3339("2025-01-01T12:00:00Z")
                .unwrap()
                .into(),
        };

        let msg = format_notification(&rule, dec!(0.75), &update);

        assert!(msg.contains("Price Alert"));
        assert!(msg.contains("Will X happen?"));
        assert!(msg.contains("Above"));
        assert!(msg.contains("70"));
        assert!(msg.contains("0.75"));
    }
}
