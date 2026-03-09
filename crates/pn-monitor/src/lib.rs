//! Price monitoring service for poly-notifier.
//!
//! This crate provides the [`MonitorService`] which maintains a live
//! WebSocket connection to the Polymarket CLOB stream, re-subscribing
//! automatically when the set of active market subscriptions changes,
//! and broadcasting [`PriceUpdate`] events to any number of downstream
//! consumers via a [`tokio::sync::broadcast`] channel.
//!
//! # Components
//!
//! - [`aggregator::SubscriptionAggregator`] – queries the database for the
//!   current set of active token IDs and the `token_id → condition_id` map.
//! - [`manager::ConnectionManager`] – owns one WebSocket connection lifetime,
//!   including the subscribe handshake, message fan-out, and exponential-
//!   backoff reconnect on disconnect.
//! - [`service::MonitorService`] – top-level orchestrator that periodically
//!   refreshes the subscription set and restarts the connection manager when
//!   the set changes.

pub mod aggregator;
pub mod manager;
pub mod service;

pub use pn_common::config::MonitorConfig;
pub use service::MonitorService;
