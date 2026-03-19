use rust_decimal::Decimal;

use crate::types::{QuoteIntent, RuntimeState};

#[derive(Debug, Clone)]
pub struct DecisionConfig {
    pub quote_size: Decimal,
    pub min_spread: Decimal,
    pub min_depth: Decimal,
    pub quote_offset_ticks: u32,
    pub min_usdc_balance: Decimal,
    pub min_token_balance: Decimal,
}

#[derive(Debug, Clone)]
pub struct DecisionOutcome {
    pub desired_quotes: Vec<QuoteIntent>,
    pub cancel_all: bool,
    pub reason: String,
}

pub struct DecisionEngine {
    config: DecisionConfig,
}

impl DecisionEngine {
    pub fn new(config: DecisionConfig) -> Self {
        Self { config }
    }

    pub fn evaluate(&self, state: &RuntimeState) -> DecisionOutcome {
        if state.flags.paused || state.flags.flattening {
            return DecisionOutcome {
                desired_quotes: Vec::new(),
                cancel_all: true,
                reason: "quoting paused".to_string(),
            };
        }

        if !state.flags.heartbeat_healthy
            || !state.flags.market_feed_healthy
            || !state.flags.user_feed_healthy
        {
            return DecisionOutcome {
                desired_quotes: Vec::new(),
                cancel_all: true,
                reason: "runtime unhealthy".to_string(),
            };
        }

        if !state.active_signals_allow_quoting() {
            return DecisionOutcome {
                desired_quotes: Vec::new(),
                cancel_all: true,
                reason: "signal gate blocked quoting".to_string(),
            };
        }

        let mut desired_quotes = Vec::new();

        for token in &state.market.tokens {
            let Some(book) = state.books.get(&token.asset_id) else {
                continue;
            };
            let (Some(best_bid), Some(best_ask)) = (book.best_bid(), book.best_ask()) else {
                continue;
            };

            let spread = best_ask.price - best_bid.price;
            if spread < self.config.min_spread || book.min_top_depth() < self.config.min_depth {
                continue;
            }

            let tick_offset = token.tick_size * Decimal::from(self.config.quote_offset_ticks);
            let buy_price = (best_bid.price + tick_offset).min(best_ask.price - token.tick_size);
            let sell_price = (best_ask.price - tick_offset).max(best_bid.price + token.tick_size);

            if buy_price >= sell_price {
                continue;
            }

            if state.account.usdc_balance >= self.config.min_usdc_balance {
                desired_quotes.push(QuoteIntent {
                    asset_id: token.asset_id.clone(),
                    side: crate::types::QuoteSide::Buy,
                    price: buy_price,
                    size: self.config.quote_size,
                    reason: "book quote".to_string(),
                });
            }

            if state.account.token_balance(&token.asset_id) >= self.config.min_token_balance {
                desired_quotes.push(QuoteIntent {
                    asset_id: token.asset_id.clone(),
                    side: crate::types::QuoteSide::Sell,
                    price: sell_price,
                    size: self.config.quote_size,
                    reason: "book quote".to_string(),
                });
            }
        }

        let cancel_all = desired_quotes.is_empty() && !state.open_orders.is_empty();
        let reason = if desired_quotes.is_empty() {
            "no quoteable books".to_string()
        } else {
            "quotes generated".to_string()
        };

        DecisionOutcome {
            desired_quotes,
            cancel_all,
            reason,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::Utc;
    use rust_decimal_macros::dec;

    use super::{DecisionConfig, DecisionEngine};
    use crate::types::{
        AccountSnapshot, BookLevel, BookSnapshot, MarketMetadata, PositionSnapshot, RuntimeFlags,
        RuntimeState, SignalState, TokenMetadata,
    };

    fn test_state() -> RuntimeState {
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
                    received_at: now,
                },
            )]),
            open_orders: Vec::new(),
            positions: HashMap::from([(
                "asset-yes".to_string(),
                PositionSnapshot {
                    asset_id: "asset-yes".to_string(),
                    size: dec!(25),
                    avg_price: dec!(0.42),
                },
            )]),
            account: AccountSnapshot {
                usdc_balance: dec!(500),
                token_balances: HashMap::from([("asset-yes".to_string(), dec!(40))]),
                updated_at: now,
            },
            signals: HashMap::from([
                (
                    "orderbook".to_string(),
                    SignalState {
                        active: true,
                        reason: "book healthy".to_string(),
                    },
                ),
                (
                    "external".to_string(),
                    SignalState {
                        active: true,
                        reason: "stub signal".to_string(),
                    },
                ),
            ]),
            flags: RuntimeFlags::default(),
            last_market_event_at: Some(now),
            last_user_event_at: Some(now),
            last_heartbeat_at: Some(now),
            last_heartbeat_id: Some("hb-1".to_string()),
            last_decision_reason: None,
        }
    }

    #[test]
    fn generates_bid_and_ask_when_market_is_quoteable() {
        let engine = DecisionEngine::new(DecisionConfig {
            quote_size: dec!(10),
            min_spread: dec!(0.01),
            min_depth: dec!(20),
            quote_offset_ticks: 1,
            min_usdc_balance: dec!(50),
            min_token_balance: dec!(10),
        });

        let outcome = engine.evaluate(&test_state());

        assert!(!outcome.cancel_all);
        assert_eq!(outcome.desired_quotes.len(), 2);
        assert_eq!(outcome.desired_quotes[0].size, dec!(10));
        assert_eq!(outcome.desired_quotes[1].size, dec!(10));
    }

    #[test]
    fn cancels_everything_when_manual_pause_or_signal_block_is_active() {
        let engine = DecisionEngine::new(DecisionConfig {
            quote_size: dec!(10),
            min_spread: dec!(0.01),
            min_depth: dec!(20),
            quote_offset_ticks: 1,
            min_usdc_balance: dec!(50),
            min_token_balance: dec!(10),
        });
        let mut state = test_state();
        state.flags.paused = true;
        state.open_orders = vec![];
        state.signals.insert(
            "external".to_string(),
            SignalState {
                active: false,
                reason: "kill switch".to_string(),
            },
        );

        let outcome = engine.evaluate(&state);

        assert!(outcome.cancel_all);
        assert!(outcome.desired_quotes.is_empty());
    }
}
