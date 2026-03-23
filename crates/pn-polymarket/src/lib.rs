//! Polymarket API client library.
//!
//! Provides three clients:
//! - [`GammaClient`] – Gamma REST API for market discovery.
//! - [`ClobClient`] – CLOB REST API for price queries.
//! - [`PolymarketWs`] – WebSocket client for real-time price updates.
//!
//! # Example
//!
//! ```no_run
//! use pn_polymarket::gamma::GammaClient;
//!
//! # #[tokio::main]
//! # async fn main() -> anyhow::Result<()> {
//! let client = GammaClient::new();
//! let markets = client.search_markets("presidential election").await?;
//! for m in &markets {
//!     println!("{} – {}", m.condition_id, m.question);
//! }
//! # Ok(())
//! # }
//! ```

pub mod clob;
pub mod gamma;
pub mod lp;
pub mod rewards;
pub mod types;
pub mod ws;

pub use clob::ClobClient;
pub use gamma::GammaClient;
pub use lp::{
    AccountSnapshot, ApprovalCheck, ApprovalStatus, ApprovalTarget, BookLevel, BookSnapshot,
    BootstrapState, ExecutionConfig, FlattenRequest, ManagedOrder, MarketMetadata,
    PolymarketExecutionClient, PositionSnapshot, QuoteRequest, QuoteSide, ReconciliationState,
    StreamEvent, TokenMetadata, TradeFill,
};
pub use rewards::{RewardClient, RewardSnapshot};
pub use types::{GammaMarket, GammaToken, WsEvent};
pub use ws::PolymarketWs;
