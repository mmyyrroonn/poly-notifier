//! `pn-notify` – Telegram notification dispatcher for poly-notifier.
//!
//! This crate provides three public modules:
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`rate_limiter`] | Per-user sliding-window rate limiting |
//! | [`telegram`] | Bot registry and HTML message delivery |
//! | [`service`] | End-to-end notification dispatch loop |
//!
//! # Quick-start
//!
//! ```no_run
//! use std::sync::Arc;
//! use tokio::sync::mpsc;
//! use tokio_util::sync::CancellationToken;
//! use pn_notify::{BotRegistry, NotifyService};
//!
//! # #[tokio::main]
//! # async fn main() -> anyhow::Result<()> {
//! let pool = sqlx::SqlitePool::connect("sqlite::memory:").await?;
//!
//! let mut registry = BotRegistry::new();
//! registry.register("1234567890:YOUR_BOT_TOKEN_HERE");
//! let registry = Arc::new(registry);
//!
//! let (tx, rx) = mpsc::channel(256);
//! let cancel = CancellationToken::new();
//!
//! let service = NotifyService::new(pool, rx, registry, 10);
//! tokio::spawn(service.run(cancel.clone()));
//! // tx.send(notification_request).await?;
//! # Ok(())
//! # }
//! ```

pub mod rate_limiter;
pub mod service;
pub mod telegram;

pub use rate_limiter::RateLimiter;
pub use service::NotifyService;
pub use telegram::BotRegistry;
