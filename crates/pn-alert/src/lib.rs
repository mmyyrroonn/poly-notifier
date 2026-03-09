//! `pn-alert` – Alert engine for the poly-notifier system.
//!
//! This crate evaluates price-based alert rules against a real-time price
//! broadcast, deduplicates notifications using a cooldown window, and forwards
//! [`NotificationRequest`]s to the notification dispatcher.
//!
//! ## Architecture
//!
//! ```text
//! pn-monitor          pn-alert              pn-notify
//! ──────────          ────────              ─────────
//! broadcast::Sender ──► AlertEngine ──► mpsc::Sender<NotificationRequest>
//!   (PriceUpdate)       │                 (NotificationRequest)
//!                       │
//!                       ├─ DashMap<token_id, Vec<AlertRule>>  (rule cache)
//!                       ├─ DashMap<token_id, Decimal>         (prev prices)
//!                       └─ AlertDedup                         (cooldowns)
//! ```
//!
//! ## Usage
//!
//! ```no_run
//! use pn_alert::AlertEngine;
//! use pn_common::config::AlertConfig;
//! use tokio::sync::{broadcast, mpsc};
//! use tokio_util::sync::CancellationToken;
//! use pn_common::events::{PriceUpdate, NotificationRequest};
//!
//! # async fn example() -> anyhow::Result<()> {
//! # let pool = sqlx::SqlitePool::connect("sqlite::memory:").await?;
//! # let (price_tx, price_rx): (tokio::sync::broadcast::Sender<PriceUpdate>, _) = tokio::sync::broadcast::channel(1024);
//! # let (notify_tx, _notify_rx): (tokio::sync::mpsc::Sender<NotificationRequest>, _) = tokio::sync::mpsc::channel(1024);
//! # let alert_cfg = AlertConfig { cache_refresh_interval_secs: 60, default_cooldown_minutes: 60 };
//! let cancel = CancellationToken::new();
//! let engine = AlertEngine::new(pool, price_rx, notify_tx, alert_cfg);
//! engine.run(cancel).await?;
//! # Ok(())
//! # }
//! ```

pub mod dedup;
pub mod engine;
pub mod rules;

pub use dedup::AlertDedup;
pub use engine::{AlertEngine, EngineConfig};
pub use rules::AlertRule;
