//! `pn-common` — shared foundation for poly-notifier.
//!
//! This crate provides:
//!
//! * [`config`] — Application configuration loaded from `config/default.toml`
//!   and environment variables.
//! * [`error`] — Unified [`error::Error`] type and [`error::Result`] alias.
//! * [`models`] — SQLx-derived database model types for every table.
//! * [`events`] — Channel event types used between monitor, alert engine, and
//!   notification dispatcher.
//! * [`db`] — Pool initialisation and common query helpers.

pub mod config;
pub mod db;
pub mod error;
pub mod events;
pub mod models;

// Re-export the most commonly needed items at the crate root for ergonomics.
pub use config::AppConfig;
pub use db::init_db;
pub use error::{Error, Result};
pub use events::{NotificationRequest, NotificationType, PriceUpdate};
pub use models::{
    Alert, AlertType, LpControlAction, LpHeartbeat, LpOrder, LpPositionSnapshot, LpReport,
    LpRiskEvent, LpTrade, Market, NotificationLog, NotificationLogType, Subscription,
    SubscriptionDetail, User, UserTier,
};
