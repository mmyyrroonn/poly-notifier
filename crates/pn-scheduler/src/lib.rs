//! `pn-scheduler` — scheduled background jobs for poly-notifier.
//!
//! Currently provides:
//!
//! * [`DailySummaryJob`] — fires on a cron schedule, queries all active
//!   user subscriptions, formats a price summary, and enqueues a
//!   [`pn_common::events::NotificationRequest`] per user.

pub mod daily_summary;

pub use daily_summary::DailySummaryJob;
