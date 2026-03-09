//! Notification dispatch service.
//!
//! [`NotifyService`] consumes [`NotificationRequest`] values from an
//! `mpsc` channel, enforces per-user rate limiting, delivers messages via
//! Telegram, retries on transient failures, and persists every attempt to
//! the `notification_log` table.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use pn_common::db::{insert_notification_log, resolve_user_id_by_telegram};
use pn_common::events::{NotificationRequest, NotificationType};
use sqlx::SqlitePool;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::rate_limiter::RateLimiter;
use crate::telegram::BotRegistry;

/// Maximum delivery attempts before a message is logged as failed.
const MAX_RETRIES: u32 = 3;

/// Delay between consecutive retry attempts.
const RETRY_DELAY: Duration = Duration::from_secs(1);

/// Main notification service.
///
/// Owns the receive end of the notification channel, the bot registry, the
/// rate limiter, and a database pool used to persist delivery records.
///
/// Call [`NotifyService::run`] to start the dispatch loop.  The loop runs
/// until the supplied [`CancellationToken`] is cancelled or the sender side of
/// the channel is dropped.
pub struct NotifyService {
    pool: SqlitePool,
    notify_rx: mpsc::Receiver<NotificationRequest>,
    bot_registry: Arc<BotRegistry>,
    rate_limiter: RateLimiter,
}

impl NotifyService {
    /// Create a new `NotifyService`.
    ///
    /// # Parameters
    ///
    /// - `pool` – SQLite connection pool with migrations already applied.
    /// - `notify_rx` – Receive end of the notification mpsc channel.
    /// - `bot_registry` – Shared bot registry pre-populated with tokens.
    /// - `max_per_minute` – Per-user rate limit applied by the internal
    ///   [`RateLimiter`].
    pub fn new(
        pool: SqlitePool,
        notify_rx: mpsc::Receiver<NotificationRequest>,
        bot_registry: Arc<BotRegistry>,
        max_per_minute: u32,
    ) -> Self {
        Self {
            pool,
            notify_rx,
            bot_registry,
            rate_limiter: RateLimiter::new(max_per_minute),
        }
    }

    /// Start the notification dispatch loop.
    ///
    /// Runs until the [`CancellationToken`] is cancelled or the sender half of
    /// the channel is dropped (channel closed).
    ///
    /// # Steps per message
    ///
    /// 1. Receive a [`NotificationRequest`] from the channel.
    /// 2. Resolve the internal `user.id` from `user_telegram_id`.
    /// 3. Check the rate limiter.
    ///    - If rate-limited: log the attempt as undelivered and skip sending.
    /// 4. Attempt delivery via [`BotRegistry::send_message`], retrying up to
    ///    [`MAX_RETRIES`] times with a [`RETRY_DELAY`] between attempts.
    /// 5. Persist the outcome to `notification_log`.
    pub async fn run(mut self, cancel_token: CancellationToken) -> Result<()> {
        info!("NotifyService started");

        loop {
            tokio::select! {
                // Honour cancellation even while waiting for the next message.
                _ = cancel_token.cancelled() => {
                    info!("NotifyService received cancellation signal, shutting down");
                    break;
                }

                msg = self.notify_rx.recv() => {
                    match msg {
                        None => {
                            // All senders dropped; no more work to do.
                            info!("notification channel closed, NotifyService shutting down");
                            break;
                        }
                        Some(req) => {
                            // Process each request; errors are captured per-message so
                            // a single failure does not abort the loop.
                            if let Err(e) = self.handle(&req).await {
                                error!(
                                    user_telegram_id = req.user_telegram_id,
                                    notification_type = %req.notification_type,
                                    error = %e,
                                    "unhandled error processing notification request"
                                );
                            }
                        }
                    }
                }
            }
        }

        info!("NotifyService stopped");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Process a single [`NotificationRequest`].
    async fn handle(&self, req: &NotificationRequest) -> Result<()> {
        debug!(
            user_telegram_id = req.user_telegram_id,
            bot_id = %req.bot_id,
            notification_type = %req.notification_type,
            "handling notification request"
        );

        // Resolve internal user ID (needed for the FK in notification_log).
        let user_id_opt = resolve_user_id_by_telegram(&self.pool, req.user_telegram_id)
            .await
            .with_context(|| {
                format!(
                    "resolving user_id for telegram_id={}",
                    req.user_telegram_id
                )
            })?;

        let Some(user_id) = user_id_opt else {
            warn!(
                user_telegram_id = req.user_telegram_id,
                "user not found in DB; delivering without logging"
            );
            // Best-effort delivery: user may have been just created on the bot
            // side and the DB row has not propagated yet.
            let _ = self.send_with_retries(req).await;
            return Ok(());
        };

        // Rate-limit check.
        if !self.rate_limiter.check_and_consume(req.user_telegram_id) {
            warn!(
                user_telegram_id = req.user_telegram_id,
                "rate limit exceeded – skipping delivery"
            );
            self.log_attempt(
                user_id,
                None,
                &req.notification_type,
                &req.message,
                false,
                Some("rate limit exceeded"),
            )
            .await?;
            return Ok(());
        }

        // Attempt delivery with retries.
        let send_result = self.send_with_retries(req).await;

        match send_result {
            Ok(()) => {
                info!(
                    user_telegram_id = req.user_telegram_id,
                    notification_type = %req.notification_type,
                    "notification delivered successfully"
                );
                self.log_attempt(
                    user_id,
                    None,
                    &req.notification_type,
                    &req.message,
                    true,
                    None,
                )
                .await?;
            }
            Err(ref e) => {
                error!(
                    user_telegram_id = req.user_telegram_id,
                    notification_type = %req.notification_type,
                    error = %e,
                    "notification delivery failed after all retries"
                );
                self.log_attempt(
                    user_id,
                    None,
                    &req.notification_type,
                    &req.message,
                    false,
                    Some(e.as_str()),
                )
                .await?;
            }
        }

        Ok(())
    }

    /// Attempt to send a message, retrying on failure up to [`MAX_RETRIES`]
    /// times.  Returns the last error string on permanent failure.
    async fn send_with_retries(&self, req: &NotificationRequest) -> Result<(), String> {
        let mut last_error = String::new();

        for attempt in 1..=MAX_RETRIES {
            match self
                .bot_registry
                .send_message(&req.bot_id, req.user_telegram_id, &req.message)
                .await
            {
                Ok(()) => return Ok(()),
                Err(e) => {
                    last_error = e;
                    if attempt < MAX_RETRIES {
                        warn!(
                            user_telegram_id = req.user_telegram_id,
                            attempt = attempt,
                            max = MAX_RETRIES,
                            error = %last_error,
                            "delivery attempt failed, will retry"
                        );
                        tokio::time::sleep(RETRY_DELAY).await;
                    }
                }
            }
        }

        Err(last_error)
    }

    /// Persist one delivery attempt to `notification_log` via the shared
    /// `pn-common` helper so the query is defined in a single place.
    async fn log_attempt(
        &self,
        user_id: i64,
        alert_id: Option<i64>,
        notification_type: &NotificationType,
        message: &str,
        delivered: bool,
        error_message: Option<&str>,
    ) -> Result<()> {
        let notification_type_str = notification_type.to_string();

        insert_notification_log(
            &self.pool,
            user_id,
            alert_id,
            &notification_type_str,
            message,
            delivered,
            error_message,
        )
        .await
        .with_context(|| {
            format!(
                "inserting notification_log for user_id={user_id}, type={notification_type_str}"
            )
        })?;

        debug!(
            user_id = user_id,
            notification_type = %notification_type_str,
            delivered = delivered,
            "notification_log row inserted"
        );

        Ok(())
    }
}
