use std::str::FromStr;

use rust_decimal::Decimal;

use crate::types::{QuoteIntent, RuntimeState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteMode {
    Join,
    Inside,
    Outside,
}

impl FromStr for QuoteMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "join" => Ok(Self::Join),
            "inside" => Ok(Self::Inside),
            "outside" => Ok(Self::Outside),
            other => Err(format!(
                "unsupported quote mode '{other}'; expected one of: join, inside, outside"
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DecisionConfig {
    pub quote_mode: QuoteMode,
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

            let (buy_price, sell_price) =
                self.quote_prices(best_bid.price, best_ask.price, token.tick_size);

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

    fn quote_prices(
        &self,
        best_bid_price: Decimal,
        best_ask_price: Decimal,
        tick_size: Decimal,
    ) -> (Decimal, Decimal) {
        let tick_offset = tick_size * Decimal::from(self.config.quote_offset_ticks);

        match self.config.quote_mode {
            QuoteMode::Join => (best_bid_price, best_ask_price),
            QuoteMode::Inside => (
                (best_bid_price + tick_offset).min(best_ask_price - tick_size),
                (best_ask_price - tick_offset).max(best_bid_price + tick_size),
            ),
            QuoteMode::Outside => (
                (best_bid_price - tick_offset).max(tick_size),
                (best_ask_price + tick_offset).min(Decimal::ONE - tick_size),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::Utc;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    use super::{DecisionConfig, DecisionEngine, QuoteMode};
    use crate::types::{
        AccountSnapshot, BookLevel, BookSnapshot, MarketMetadata, PositionSnapshot, RuntimeFlags,
        RuntimeState, SignalState, TokenMetadata,
    };

    fn decision_config(quote_mode: QuoteMode, quote_offset_ticks: u32) -> DecisionConfig {
        DecisionConfig {
            quote_mode,
            quote_size: dec!(10),
            min_spread: dec!(0.01),
            min_depth: dec!(20),
            quote_offset_ticks,
            min_usdc_balance: dec!(50),
            min_token_balance: dec!(10),
        }
    }

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
            flags: RuntimeFlags {
                heartbeat_healthy: true,
                market_feed_healthy: true,
                user_feed_healthy: true,
                ..RuntimeFlags::default()
            },
            last_market_event_at: Some(now),
            last_user_event_at: Some(now),
            last_heartbeat_at: Some(now),
            last_heartbeat_id: Some("hb-1".to_string()),
            last_decision_reason: None,
        }
    }

    #[test]
    fn generates_bid_and_ask_when_market_is_quoteable() {
        let engine = DecisionEngine::new(decision_config(QuoteMode::Inside, 1));

        let outcome = engine.evaluate(&test_state());

        assert!(!outcome.cancel_all);
        assert_eq!(outcome.desired_quotes.len(), 2);
        assert_eq!(outcome.desired_quotes[0].size, dec!(10));
        assert_eq!(outcome.desired_quotes[1].size, dec!(10));
    }

    #[test]
    fn generated_quotes_remain_tick_aligned() {
        let engine = DecisionEngine::new(decision_config(QuoteMode::Inside, 1));
        let state = test_state();
        let tick_size = state.market.tokens[0].tick_size;

        let outcome = engine.evaluate(&state);

        assert!(!outcome.desired_quotes.is_empty());
        for quote in outcome.desired_quotes {
            assert_eq!(quote.price % tick_size, Decimal::ZERO);
        }
    }

    #[test]
    fn generated_quotes_use_true_top_of_book_when_levels_are_unsorted() {
        let engine = DecisionEngine::new(decision_config(QuoteMode::Inside, 2));
        let mut state = test_state();
        state.books.get_mut("asset-yes").unwrap().bids = vec![
            BookLevel {
                price: dec!(0.01),
                size: dec!(100),
            },
            BookLevel {
                price: dec!(0.26),
                size: dec!(100),
            },
        ];
        state.books.get_mut("asset-yes").unwrap().asks = vec![
            BookLevel {
                price: dec!(0.99),
                size: dec!(120),
            },
            BookLevel {
                price: dec!(0.27),
                size: dec!(120),
            },
        ];

        let outcome = engine.evaluate(&state);

        assert_eq!(outcome.desired_quotes.len(), 2);
        assert_eq!(outcome.desired_quotes[0].price, dec!(0.26));
        assert_eq!(outcome.desired_quotes[1].price, dec!(0.27));
    }

    #[test]
    fn join_mode_quotes_at_top_of_book() {
        let engine = DecisionEngine::new(decision_config(QuoteMode::Join, 2));

        let outcome = engine.evaluate(&test_state());

        assert_eq!(outcome.desired_quotes.len(), 2);
        assert_eq!(outcome.desired_quotes[0].price, dec!(0.40));
        assert_eq!(outcome.desired_quotes[1].price, dec!(0.45));
    }

    #[test]
    fn outside_mode_quotes_away_from_top_of_book() {
        let engine = DecisionEngine::new(decision_config(QuoteMode::Outside, 2));

        let outcome = engine.evaluate(&test_state());

        assert_eq!(outcome.desired_quotes.len(), 2);
        assert_eq!(outcome.desired_quotes[0].price, dec!(0.38));
        assert_eq!(outcome.desired_quotes[1].price, dec!(0.47));
    }

    #[test]
    fn cancels_everything_when_manual_pause_or_signal_block_is_active() {
        let engine = DecisionEngine::new(decision_config(QuoteMode::Inside, 1));
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
