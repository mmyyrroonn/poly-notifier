//! Main monitor service orchestrating the aggregator and WebSocket manager.
//!
//! [`MonitorService`] is the top-level entry point for the price-monitoring
//! subsystem.  It:
//!
//! 1. Queries the database for the initial set of active token IDs.
//! 2. Starts a [`ConnectionManager`] task that maintains a live Polymarket
//!    WebSocket subscription for those tokens.
//! 3. Periodically re-queries the database.  If the active token set has
//!    changed, it cancels the current WS session and starts a fresh one with
//!    the updated set.
//! 4. Shuts down gracefully when the provided [`CancellationToken`] fires.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Result;
use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use pn_common::config::MonitorConfig;
use pn_common::events::PriceUpdate;

use crate::aggregator::SubscriptionAggregator;
use crate::manager::ConnectionManager;

/// The price-monitoring service.
///
/// # Example
///
/// ```no_run
/// use sqlx::SqlitePool;
/// use tokio::sync::broadcast;
/// use tokio_util::sync::CancellationToken;
/// use pn_common::config::MonitorConfig;
/// use pn_monitor::service::MonitorService;
///
/// # #[tokio::main]
/// # async fn main() -> anyhow::Result<()> {
/// let pool = SqlitePool::connect("sqlite::memory:").await?;
/// let (price_tx, _) = broadcast::channel(1024);
/// let config = MonitorConfig {
///     subscription_refresh_interval_secs: 60,
///     ws_ping_interval_secs: 10,
///     reconnect_base_delay_secs: 1,
///     reconnect_max_delay_secs: 60,
/// };
/// let svc = MonitorService::new(pool, price_tx, config);
/// let cancel = CancellationToken::new();
/// // svc.run(cancel).await?;
/// # Ok(())
/// # }
/// ```
pub struct MonitorService {
    pool: SqlitePool,
    price_tx: broadcast::Sender<PriceUpdate>,
    config: MonitorConfig,
}

impl MonitorService {
    /// Create a new [`MonitorService`].
    ///
    /// - `pool` – SQLite connection pool used by the aggregator.
    /// - `price_tx` – broadcast sender on which enriched [`PriceUpdate`]s are
    ///   published.
    /// - `config` – timing parameters for refresh and WS ping/reconnect.
    pub fn new(
        pool: SqlitePool,
        price_tx: broadcast::Sender<PriceUpdate>,
        config: MonitorConfig,
    ) -> Self {
        Self {
            pool,
            price_tx,
            config,
        }
    }

    /// Run the monitor service until `cancel` is triggered.
    ///
    /// # Steps
    ///
    /// 1. Build a [`SubscriptionAggregator`] and fetch the initial token set.
    /// 2. Spawn a [`ConnectionManager`] task for that token set.
    /// 3. Every `subscription_refresh_interval_secs`, re-query the database.
    ///    If the token set has changed:
    ///    - Cancel the current connection manager task.
    ///    - Wait for it to exit.
    ///    - Start a new connection manager with the updated set.
    /// 4. On `cancel`: stop the active connection manager and return `Ok(())`.
    ///
    /// # Errors
    ///
    /// Database query errors during refresh are logged and skipped; they do
    /// not terminate the service.  This method only returns an error for
    /// unrecoverable internal failures.
    pub async fn run(self, cancel: CancellationToken) -> Result<()> {
        let aggregator = SubscriptionAggregator::new(self.pool.clone());
        let refresh_interval = Duration::from_secs(self.config.subscription_refresh_interval_secs);
        let ws_ping = Duration::from_secs(self.config.ws_ping_interval_secs);
        let reconnect_base = Duration::from_secs(self.config.reconnect_base_delay_secs);
        let reconnect_max = Duration::from_secs(self.config.reconnect_max_delay_secs);

        info!(
            refresh_secs = self.config.subscription_refresh_interval_secs,
            "MonitorService: starting"
        );

        // Fetch the initial token set (with retry on error).
        let mut current_ids: HashSet<String> =
            fetch_token_ids_with_retry(&aggregator, refresh_interval, &cancel).await;

        if cancel.is_cancelled() {
            info!("MonitorService: cancelled before first connection");
            return Ok(());
        }

        // Spawn the first connection manager task.
        let (mut conn_cancel, mut conn_handle) = spawn_connection(
            current_ids.iter().cloned().collect(),
            &aggregator,
            self.price_tx.clone(),
            ws_ping,
            reconnect_base,
            reconnect_max,
        )
        .await;

        // Subscription refresh ticker.  The first tick fires immediately from
        // `interval`; we consume it so the first real check happens after one
        // full interval has elapsed.
        let mut ticker = tokio::time::interval(refresh_interval);
        ticker.tick().await;

        loop {
            tokio::select! {
                biased;

                // ── Global shutdown ──────────────────────────────────────
                () = cancel.cancelled() => {
                    info!("MonitorService: shutdown requested");
                    conn_cancel.cancel();
                    let _ = conn_handle.await;
                    info!("MonitorService: stopped");
                    return Ok(());
                }

                // ── Connection task exited unexpectedly ──────────────────
                // Under normal operation this shouldn't happen because
                // ConnectionManager loops until its own cancel token fires.
                // Handle it defensively.
                _ = &mut conn_handle => {
                    warn!("MonitorService: connection task exited unexpectedly, restarting");
                    // Refresh the token set before restarting.
                    let new_ids =
                        fetch_token_ids_with_retry(&aggregator, refresh_interval, &cancel).await;
                    if cancel.is_cancelled() {
                        return Ok(());
                    }
                    current_ids = new_ids;
                    let (new_cancel, new_handle) = spawn_connection(
                        current_ids.iter().cloned().collect(),
                        &aggregator,
                        self.price_tx.clone(),
                        ws_ping,
                        reconnect_base,
                        reconnect_max,
                    )
                    .await;
                    conn_cancel = new_cancel;
                    conn_handle = new_handle;
                }

                // ── Periodic subscription refresh ────────────────────────
                _ = ticker.tick() => {
                    let new_ids = match aggregator.get_active_token_ids().await {
                        Ok(ids) => ids,
                        Err(e) => {
                            error!(error=%e, "MonitorService: failed to refresh token IDs");
                            continue;
                        }
                    };

                    if new_ids == current_ids {
                        // No change; keep the existing connection running.
                        continue;
                    }

                    info!(
                        old = current_ids.len(),
                        new = new_ids.len(),
                        "MonitorService: token set changed, restarting WS connection"
                    );

                    // Stop the current connection and start a fresh one.
                    conn_cancel.cancel();
                    let _ = conn_handle.await;

                    current_ids = new_ids;
                    let (new_cancel, new_handle) = spawn_connection(
                        current_ids.iter().cloned().collect(),
                        &aggregator,
                        self.price_tx.clone(),
                        ws_ping,
                        reconnect_base,
                        reconnect_max,
                    )
                    .await;
                    conn_cancel = new_cancel;
                    conn_handle = new_handle;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Private free functions
// ---------------------------------------------------------------------------

/// Spawn a [`ConnectionManager`] task, returning its child cancel token and
/// join handle.
///
/// The child cancel token is independent of the service's main cancel token so
/// that we can stop just the connection (on token-set change) without shutting
/// down the whole service.
async fn spawn_connection(
    token_ids: Vec<String>,
    aggregator: &SubscriptionAggregator,
    price_tx: broadcast::Sender<PriceUpdate>,
    ws_ping: Duration,
    reconnect_base: Duration,
    reconnect_max: Duration,
) -> (CancellationToken, tokio::task::JoinHandle<()>) {
    // Build the token→condition enrichment map.
    let token_to_condition = match aggregator.get_token_to_condition().await {
        Ok(map) => map,
        Err(e) => {
            warn!(
                error=%e,
                "MonitorService: could not build token→condition map, using empty"
            );
            std::collections::HashMap::new()
        }
    };

    let child_cancel = CancellationToken::new();
    let task_cancel = child_cancel.clone();

    let handle = tokio::spawn(async move {
        let mgr = ConnectionManager::new(ws_ping, reconnect_base, reconnect_max);
        mgr.run(token_ids, token_to_condition, price_tx, task_cancel)
            .await;
    });

    (child_cancel, handle)
}

/// Attempt to fetch active token IDs from the aggregator, retrying on error
/// until success or until `cancel` fires.
///
/// Returns an empty set if cancelled before the first successful fetch.
async fn fetch_token_ids_with_retry(
    aggregator: &SubscriptionAggregator,
    retry_interval: Duration,
    cancel: &CancellationToken,
) -> HashSet<String> {
    loop {
        match aggregator.get_active_token_ids().await {
            Ok(ids) => {
                info!(count = ids.len(), "MonitorService: fetched token set from DB");
                return ids;
            }
            Err(e) => {
                error!(
                    error=%e,
                    retry_secs = retry_interval.as_secs(),
                    "MonitorService: failed to fetch token IDs, retrying"
                );
                tokio::select! {
                    () = tokio::time::sleep(retry_interval) => {}
                    () = cancel.cancelled() => return HashSet::new(),
                }
            }
        }
    }
}
