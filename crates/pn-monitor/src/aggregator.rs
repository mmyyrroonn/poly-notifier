//! Database-backed aggregation of active token subscriptions.
//!
//! [`SubscriptionAggregator`] queries the SQLite database to determine which
//! CLOB token IDs currently have at least one active subscription, and builds
//! the `token_id → condition_id` lookup used to enrich price updates.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use sqlx::SqlitePool;
use tracing::{debug, instrument};

/// Aggregates the set of token IDs that require live price monitoring.
///
/// The aggregator reads from the database on every call; it holds no
/// in-memory cache itself so that the caller (the service) controls when
/// to refresh and can detect set changes.
///
/// # Example
///
/// ```no_run
/// use sqlx::SqlitePool;
/// use pn_monitor::aggregator::SubscriptionAggregator;
///
/// # #[tokio::main]
/// # async fn main() -> anyhow::Result<()> {
/// # let pool = SqlitePool::connect("sqlite::memory:").await?;
/// let agg = SubscriptionAggregator::new(pool);
/// let token_ids = agg.get_active_token_ids().await?;
/// println!("monitoring {} tokens", token_ids.len());
/// # Ok(())
/// # }
/// ```
pub struct SubscriptionAggregator {
    pool: SqlitePool,
}

impl SubscriptionAggregator {
    /// Create a new [`SubscriptionAggregator`] backed by `pool`.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Return the set of all distinct CLOB token IDs that have at least one
    /// active subscription pointing to an active market.
    ///
    /// Internally this executes:
    ///
    /// ```sql
    /// SELECT DISTINCT m.token_ids
    /// FROM markets m
    /// INNER JOIN subscriptions s ON s.market_id = m.id
    /// WHERE s.is_active = 1 AND m.is_active = 1
    /// ```
    ///
    /// Each row's `token_ids` value is a JSON array of strings (e.g.
    /// `["0xabc","0xdef"]`).  These arrays are parsed and flattened into the
    /// returned [`HashSet`].
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails or any `token_ids` JSON
    /// value cannot be parsed.
    #[instrument(skip(self), name = "aggregator::get_active_token_ids")]
    pub async fn get_active_token_ids(&self) -> Result<HashSet<String>> {
        // Fetch distinct token_ids JSON strings from markets with active subs.
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT m.token_ids \
             FROM markets m \
             INNER JOIN subscriptions s ON s.market_id = m.id \
             WHERE s.is_active = 1 AND m.is_active = 1",
        )
        .fetch_all(&self.pool)
        .await
        .context("querying active token_ids from DB")?;

        let mut token_ids = HashSet::new();

        for (json_str,) in rows {
            // Each value is a JSON array like `["0xabc","0xdef"]`.
            let ids: Vec<String> = serde_json::from_str(&json_str)
                .with_context(|| format!("parsing token_ids JSON: {json_str}"))?;
            token_ids.extend(ids);
        }

        debug!(count = token_ids.len(), "active token IDs aggregated");
        Ok(token_ids)
    }

    /// Return a map of `token_id → condition_id` for all active markets.
    ///
    /// This map is used by the connection manager to enrich raw WebSocket
    /// price events (which carry only `token_id`) with the `condition_id`
    /// needed by downstream alert consumers.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails or any JSON column cannot
    /// be parsed.
    #[instrument(skip(self), name = "aggregator::get_token_to_condition")]
    pub async fn get_token_to_condition(&self) -> Result<HashMap<String, String>> {
        // Fetch condition_id and token_ids for all active markets that have
        // at least one active subscription.
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT DISTINCT m.condition_id, m.token_ids \
             FROM markets m \
             INNER JOIN subscriptions s ON s.market_id = m.id \
             WHERE s.is_active = 1 AND m.is_active = 1",
        )
        .fetch_all(&self.pool)
        .await
        .context("querying token_to_condition mapping from DB")?;

        let mut mapping = HashMap::new();

        for (condition_id, token_ids_json) in rows {
            let ids: Vec<String> = serde_json::from_str(&token_ids_json).with_context(|| {
                format!("parsing token_ids JSON for condition {condition_id}: {token_ids_json}")
            })?;
            for token_id in ids {
                mapping.insert(token_id, condition_id.clone());
            }
        }

        debug!(count = mapping.len(), "token→condition mapping built");
        Ok(mapping)
    }
}
