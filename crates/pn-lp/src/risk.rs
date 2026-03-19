use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;

use crate::types::{QuoteSide, RuntimeState, TradeFill};

#[derive(Debug, Clone)]
pub struct RiskConfig {
    pub max_position: Decimal,
    pub flat_position_tolerance: Decimal,
    pub stale_feed_after: Duration,
    pub auto_flatten_after_fill: bool,
    pub flatten_use_fok: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FlattenIntent {
    pub asset_id: String,
    pub side: QuoteSide,
    pub size: Decimal,
    pub use_fok: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RiskAction {
    None,
    Pause { reason: String },
    Resume { reason: String },
    CancelAll { reason: String },
    Flatten(FlattenIntent),
}

pub struct RiskEngine {
    config: RiskConfig,
}

impl RiskEngine {
    pub fn new(config: RiskConfig) -> Self {
        Self { config }
    }

    pub fn on_fill(&self, fill: &TradeFill) -> Vec<RiskAction> {
        let mut actions = vec![
            RiskAction::Pause {
                reason: "fill detected".to_string(),
            },
            RiskAction::CancelAll {
                reason: "fill detected".to_string(),
            },
        ];

        if self.config.auto_flatten_after_fill {
            actions.push(RiskAction::Flatten(FlattenIntent {
                asset_id: fill.asset_id.clone(),
                side: fill.side.opposite(),
                size: fill.size,
                use_fok: self.config.flatten_use_fok,
            }));
        }

        actions
    }

    pub fn on_timer(&self, state: &RuntimeState, now: DateTime<Utc>) -> Vec<RiskAction> {
        if let Some(last_market_event_at) = state.last_market_event_at {
            if now - last_market_event_at > self.config.stale_feed_after {
                return vec![
                    RiskAction::Pause {
                        reason: "market feed stale".to_string(),
                    },
                    RiskAction::CancelAll {
                        reason: "market feed stale".to_string(),
                    },
                ];
            }
        }

        if let Some(last_user_event_at) = state.last_user_event_at {
            if now - last_user_event_at > self.config.stale_feed_after {
                return vec![
                    RiskAction::Pause {
                        reason: "user feed stale".to_string(),
                    },
                    RiskAction::CancelAll {
                        reason: "user feed stale".to_string(),
                    },
                ];
            }
        }

        let out_of_bounds = state
            .positions
            .values()
            .any(|position| position.size.abs() > self.config.max_position);
        if out_of_bounds {
            return vec![
                RiskAction::Pause {
                    reason: "position limit breached".to_string(),
                },
                RiskAction::CancelAll {
                    reason: "position limit breached".to_string(),
                },
            ];
        }

        if state.flags.paused
            && !state.flags.flattening
            && state.open_orders.is_empty()
            && state
                .positions
                .values()
                .all(|position| position.size.abs() <= self.config.flat_position_tolerance)
        {
            return vec![RiskAction::Resume {
                reason: "reconciled flat state".to_string(),
            }];
        }

        vec![RiskAction::None]
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::{Duration, Utc};
    use rust_decimal_macros::dec;

    use super::{FlattenIntent, RiskAction, RiskConfig, RiskEngine};
    use crate::types::{
        AccountSnapshot, BookLevel, BookSnapshot, MarketMetadata, PositionSnapshot, QuoteSide,
        RuntimeFlags, RuntimeState, SignalState, TokenMetadata, TradeFill,
    };

    fn engine() -> RiskEngine {
        RiskEngine::new(RiskConfig {
            max_position: dec!(100),
            flat_position_tolerance: dec!(1),
            stale_feed_after: Duration::seconds(15),
            auto_flatten_after_fill: true,
            flatten_use_fok: false,
        })
    }

    fn state_with_old_market_feed() -> RuntimeState {
        let now = Utc::now();
        RuntimeState {
            market: MarketMetadata {
                condition_id: "condition-1".to_string(),
                question: "Will X happen?".to_string(),
                tokens: vec![TokenMetadata {
                    asset_id: "asset-yes".to_string(),
                    outcome: "Yes".to_string(),
                    tick_size: dec!(0.01),
                }],
            },
            books: HashMap::from([(
                "asset-yes".to_string(),
                BookSnapshot {
                    asset_id: "asset-yes".to_string(),
                    bids: vec![BookLevel {
                        price: dec!(0.40),
                        size: dec!(100),
                    }],
                    asks: vec![BookLevel {
                        price: dec!(0.45),
                        size: dec!(120),
                    }],
                    received_at: now - Duration::seconds(30),
                },
            )]),
            open_orders: Vec::new(),
            fills: Vec::new(),
            positions: HashMap::from([(
                "asset-yes".to_string(),
                PositionSnapshot {
                    asset_id: "asset-yes".to_string(),
                    size: dec!(0),
                    avg_price: dec!(0),
                },
            )]),
            account: AccountSnapshot {
                usdc_balance: dec!(500),
                token_balances: HashMap::from([("asset-yes".to_string(), dec!(40))]),
                updated_at: now,
            },
            signals: HashMap::from([(
                "orderbook".to_string(),
                SignalState {
                    active: true,
                    reason: "book healthy".to_string(),
                },
            )]),
            flags: RuntimeFlags::default(),
            last_market_event_at: Some(now - Duration::seconds(30)),
            last_user_event_at: Some(now),
            last_heartbeat_at: Some(now),
            last_heartbeat_id: Some("hb-1".to_string()),
            last_decision_reason: None,
        }
    }

    #[test]
    fn fill_triggers_cancel_pause_and_flatten() {
        let fill = TradeFill {
            trade_id: "trade-1".to_string(),
            order_id: Some("order-1".to_string()),
            asset_id: "asset-yes".to_string(),
            side: QuoteSide::Buy,
            price: dec!(0.44),
            size: dec!(12),
            status: "MATCHED".to_string(),
            received_at: Utc::now(),
        };

        let actions = engine().on_fill(&fill);

        assert_eq!(
            actions,
            vec![
                RiskAction::Pause {
                    reason: "fill detected".to_string()
                },
                RiskAction::CancelAll {
                    reason: "fill detected".to_string()
                },
                RiskAction::Flatten(FlattenIntent {
                    asset_id: "asset-yes".to_string(),
                    side: QuoteSide::Sell,
                    size: dec!(12),
                    use_fok: false,
                }),
            ]
        );
    }

    #[test]
    fn stale_market_feed_forces_pause_and_cancel() {
        let now = Utc::now();
        let actions = engine().on_timer(&state_with_old_market_feed(), now);

        assert_eq!(
            actions,
            vec![
                RiskAction::Pause {
                    reason: "market feed stale".to_string()
                },
                RiskAction::CancelAll {
                    reason: "market feed stale".to_string()
                },
            ]
        );
    }
}
