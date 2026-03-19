use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum QuoteSide {
    Buy,
    Sell,
}

impl QuoteSide {
    pub fn opposite(&self) -> Self {
        match self {
            Self::Buy => Self::Sell,
            Self::Sell => Self::Buy,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenMetadata {
    pub asset_id: String,
    pub outcome: String,
    pub tick_size: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketMetadata {
    pub condition_id: String,
    pub question: String,
    pub tokens: Vec<TokenMetadata>,
}

impl MarketMetadata {
    pub fn token(&self, asset_id: &str) -> Option<&TokenMetadata> {
        self.tokens.iter().find(|token| token.asset_id == asset_id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookLevel {
    pub price: Decimal,
    pub size: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookSnapshot {
    pub asset_id: String,
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
    pub received_at: DateTime<Utc>,
}

impl BookSnapshot {
    pub fn best_bid(&self) -> Option<&BookLevel> {
        self.bids.first()
    }

    pub fn best_ask(&self) -> Option<&BookLevel> {
        self.asks.first()
    }

    pub fn min_top_depth(&self) -> Decimal {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) => bid.size.min(ask.size),
            _ => Decimal::ZERO,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedOrder {
    pub order_id: String,
    pub asset_id: String,
    pub side: QuoteSide,
    pub price: Decimal,
    pub size: Decimal,
    pub created_at: DateTime<Utc>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeFill {
    pub trade_id: String,
    pub order_id: Option<String>,
    pub asset_id: String,
    pub side: QuoteSide,
    pub price: Decimal,
    pub size: Decimal,
    pub status: String,
    pub received_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionSnapshot {
    pub asset_id: String,
    pub size: Decimal,
    pub avg_price: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountSnapshot {
    pub usdc_balance: Decimal,
    pub token_balances: HashMap<String, Decimal>,
    pub updated_at: DateTime<Utc>,
}

impl AccountSnapshot {
    pub fn token_balance(&self, asset_id: &str) -> Decimal {
        self.token_balances
            .get(asset_id)
            .copied()
            .unwrap_or(Decimal::ZERO)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuoteIntent {
    pub asset_id: String,
    pub side: QuoteSide,
    pub price: Decimal,
    pub size: Decimal,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeFlags {
    pub paused: bool,
    pub flattening: bool,
    pub heartbeat_healthy: bool,
    pub market_feed_healthy: bool,
    pub user_feed_healthy: bool,
}

impl Default for RuntimeFlags {
    fn default() -> Self {
        Self {
            paused: false,
            flattening: false,
            heartbeat_healthy: true,
            market_feed_healthy: true,
            user_feed_healthy: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalState {
    pub active: bool,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeState {
    pub market: MarketMetadata,
    pub books: HashMap<String, BookSnapshot>,
    pub open_orders: Vec<ManagedOrder>,
    pub positions: HashMap<String, PositionSnapshot>,
    pub account: AccountSnapshot,
    pub signals: HashMap<String, SignalState>,
    pub flags: RuntimeFlags,
    pub last_market_event_at: Option<DateTime<Utc>>,
    pub last_user_event_at: Option<DateTime<Utc>>,
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    pub last_heartbeat_id: Option<String>,
    pub last_decision_reason: Option<String>,
}

impl RuntimeState {
    pub fn active_signals_allow_quoting(&self) -> bool {
        self.signals.values().all(|signal| signal.active)
    }
}
