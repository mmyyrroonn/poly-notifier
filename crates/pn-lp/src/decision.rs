use std::str::FromStr;

use chrono::Utc;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;

use crate::types::{BookLevel, BookSnapshot, QuoteIntent, QuoteSide, RewardSnapshot, RuntimeState};

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
    pub min_inside_ticks: u32,
    pub min_inside_depth_multiple: Decimal,
    pub reward_stale_after: chrono::Duration,
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

#[derive(Debug, Clone)]
struct CandidateQuote {
    quote: QuoteIntent,
    inside_ticks: u32,
    inside_depth: Decimal,
    inventory_bias: Decimal,
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

        let now = Utc::now();
        let Some(reward) = state.reward.active_snapshot(
            now,
            self.config.reward_stale_after,
            &state.market.condition_id,
        ) else {
            let cancel_all = !state.open_orders.is_empty();
            return DecisionOutcome {
                desired_quotes: Vec::new(),
                cancel_all,
                reason: "reward state inactive".to_string(),
            };
        };

        let mut candidates = Vec::new();
        for token in &state.market.tokens {
            candidates.extend(self.reward_candidates(state, token, reward));
        }

        let desired_quotes = candidates
            .into_iter()
            .max_by(|left, right| compare_candidates(left, right))
            .map(|candidate| vec![candidate.quote])
            .unwrap_or_default();
        let cancel_all = desired_quotes.is_empty() && !state.open_orders.is_empty();
        let reason = if desired_quotes.is_empty() {
            "no reward-qualified quotes".to_string()
        } else {
            "reward-qualified single-sided quote".to_string()
        };

        DecisionOutcome {
            desired_quotes,
            cancel_all,
            reason,
        }
    }

    fn reward_candidates(
        &self,
        state: &RuntimeState,
        token: &crate::types::TokenMetadata,
        reward: &RewardSnapshot,
    ) -> Vec<CandidateQuote> {
        let Some(book) = state.books.get(&token.asset_id) else {
            return Vec::new();
        };
        let (Some(best_bid), Some(best_ask)) = (book.best_bid(), book.best_ask()) else {
            return Vec::new();
        };

        let spread = best_ask.price - best_bid.price;
        if spread < self.config.min_spread || book.min_top_depth() < self.config.min_depth {
            return Vec::new();
        }

        let Some(adjusted_midpoint) = adjusted_midpoint(book, reward.min_size) else {
            return Vec::new();
        };
        if adjusted_midpoint < Decimal::new(10, 2) || adjusted_midpoint > Decimal::new(90, 2) {
            return Vec::new();
        }

        let mut candidates = Vec::new();
        if state.account.usdc_balance >= self.config.min_usdc_balance {
            if let Some(price) =
                qualifying_bid_price(adjusted_midpoint, reward.max_spread, token.tick_size)
            {
                if let Some(candidate) = self.build_candidate(
                    state,
                    token.asset_id.as_str(),
                    book,
                    QuoteSide::Buy,
                    price,
                    token.tick_size,
                ) {
                    candidates.push(candidate);
                }
            }
        }

        if state.account.token_balance(&token.asset_id) >= self.config.min_token_balance {
            if let Some(price) =
                qualifying_ask_price(adjusted_midpoint, reward.max_spread, token.tick_size)
            {
                if let Some(candidate) = self.build_candidate(
                    state,
                    token.asset_id.as_str(),
                    book,
                    QuoteSide::Sell,
                    price,
                    token.tick_size,
                ) {
                    candidates.push(candidate);
                }
            }
        }

        candidates
    }

    fn build_candidate(
        &self,
        state: &RuntimeState,
        asset_id: &str,
        book: &BookSnapshot,
        side: QuoteSide,
        price: Decimal,
        tick_size: Decimal,
    ) -> Option<CandidateQuote> {
        let inside_ticks = inside_ticks(book, side.clone(), price, tick_size);
        let inside_depth = inside_depth(book, side.clone(), price);
        if inside_ticks < self.config.min_inside_ticks {
            return None;
        }
        if inside_depth < self.config.quote_size * self.config.min_inside_depth_multiple {
            return None;
        }

        let inventory_bias = match side {
            QuoteSide::Sell => state.account.token_balance(asset_id),
            QuoteSide::Buy => Decimal::ZERO,
        };

        Some(CandidateQuote {
            quote: QuoteIntent {
                asset_id: asset_id.to_string(),
                side,
                price,
                size: self.config.quote_size,
                reason: "reward-qualified quote".to_string(),
            },
            inside_ticks,
            inside_depth,
            inventory_bias,
        })
    }
}

fn compare_candidates(left: &CandidateQuote, right: &CandidateQuote) -> std::cmp::Ordering {
    left.inside_ticks
        .cmp(&right.inside_ticks)
        .then_with(|| left.inside_depth.cmp(&right.inside_depth))
        .then_with(|| left.inventory_bias.cmp(&right.inventory_bias))
        .then_with(|| match (&left.quote.side, &right.quote.side) {
            (QuoteSide::Sell, QuoteSide::Buy) => std::cmp::Ordering::Greater,
            (QuoteSide::Buy, QuoteSide::Sell) => std::cmp::Ordering::Less,
            _ => std::cmp::Ordering::Equal,
        })
}

fn adjusted_midpoint(book: &BookSnapshot, min_size: Decimal) -> Option<Decimal> {
    let bid = price_at_cumulative_depth(&book.bids, min_size, true)?;
    let ask = price_at_cumulative_depth(&book.asks, min_size, false)?;
    Some((bid + ask) / Decimal::from(2u32))
}

fn price_at_cumulative_depth(
    levels: &[BookLevel],
    min_size: Decimal,
    descending: bool,
) -> Option<Decimal> {
    let mut sorted = levels.to_vec();
    if descending {
        sorted.sort_by(|left, right| right.price.cmp(&left.price));
    } else {
        sorted.sort_by(|left, right| left.price.cmp(&right.price));
    }

    let mut cumulative = Decimal::ZERO;
    for level in sorted {
        cumulative += level.size;
        if cumulative >= min_size {
            return Some(level.price);
        }
    }
    None
}

fn qualifying_bid_price(
    adjusted_midpoint: Decimal,
    max_spread: Decimal,
    tick_size: Decimal,
) -> Option<Decimal> {
    let price = adjusted_midpoint - max_spread;
    let aligned = round_up_to_tick(price.max(tick_size), tick_size);
    (aligned < Decimal::ONE).then_some(aligned)
}

fn qualifying_ask_price(
    adjusted_midpoint: Decimal,
    max_spread: Decimal,
    tick_size: Decimal,
) -> Option<Decimal> {
    let price = adjusted_midpoint + max_spread;
    let aligned = round_down_to_tick(price.min(Decimal::ONE - tick_size), tick_size);
    (aligned > Decimal::ZERO).then_some(aligned)
}

fn round_up_to_tick(price: Decimal, tick_size: Decimal) -> Decimal {
    let remainder = price % tick_size;
    let mut aligned = if remainder.is_zero() {
        price
    } else {
        price + (tick_size - remainder)
    };
    aligned.rescale(tick_size.scale());
    aligned
}

fn round_down_to_tick(price: Decimal, tick_size: Decimal) -> Decimal {
    let mut aligned = price - (price % tick_size);
    aligned.rescale(tick_size.scale());
    aligned
}

fn inside_ticks(book: &BookSnapshot, side: QuoteSide, price: Decimal, tick_size: Decimal) -> u32 {
    let distance = match side {
        QuoteSide::Buy => book
            .bids
            .iter()
            .filter(|level| level.price > price)
            .map(|level| level.price - price)
            .max(),
        QuoteSide::Sell => book
            .asks
            .iter()
            .filter(|level| level.price < price)
            .map(|level| price - level.price)
            .max(),
    };

    distance
        .map(|value| (value / tick_size).trunc().to_u32().unwrap_or(0))
        .unwrap_or(0)
}

fn inside_depth(book: &BookSnapshot, side: QuoteSide, price: Decimal) -> Decimal {
    match side {
        QuoteSide::Buy => book
            .bids
            .iter()
            .filter(|level| level.price > price)
            .fold(Decimal::ZERO, |sum, level| sum + level.size),
        QuoteSide::Sell => book
            .asks
            .iter()
            .filter(|level| level.price < price)
            .fold(Decimal::ZERO, |sum, level| sum + level.size),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::{NaiveDate, Utc};
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    use super::{DecisionConfig, DecisionEngine, QuoteMode};
    use crate::types::{
        AccountSnapshot, BookLevel, BookSnapshot, MarketMetadata, PositionSnapshot, RewardSnapshot,
        RewardState, RuntimeFlags, RuntimeState, SignalState, TokenMetadata,
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
            min_inside_ticks: 1,
            min_inside_depth_multiple: dec!(1.5),
            reward_stale_after: chrono::Duration::seconds(30),
        }
    }

    fn active_reward_state(now: chrono::DateTime<Utc>, max_spread: Decimal) -> RewardState {
        RewardState {
            snapshot: Some(RewardSnapshot {
                condition_id: "condition-1".to_string(),
                max_spread,
                min_size: dec!(50),
                total_daily_rate: dec!(4.5),
                active_until: NaiveDate::from_ymd_opt(2026, 4, 1).unwrap(),
                token_prices: HashMap::from([("asset-yes".to_string(), dec!(0.505))]),
            }),
            last_attempt_at: Some(now),
            last_success_at: Some(now),
            last_error: None,
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
                        price: dec!(0.50),
                        size: dec!(100),
                    }],
                    asks: vec![BookLevel {
                        price: dec!(0.51),
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
            reward: active_reward_state(now, dec!(0.03)),
        }
    }

    #[test]
    fn generates_single_reward_qualified_quote_when_market_is_quoteable() {
        let engine = DecisionEngine::new(decision_config(QuoteMode::Inside, 1));

        let outcome = engine.evaluate(&test_state());

        assert!(!outcome.cancel_all);
        assert_eq!(outcome.desired_quotes.len(), 1);
        assert_eq!(outcome.desired_quotes[0].size, dec!(10));
        assert_eq!(
            outcome.desired_quotes[0].side,
            crate::types::QuoteSide::Sell
        );
        assert_eq!(outcome.desired_quotes[0].price, dec!(0.53));
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
    fn generated_quotes_do_not_leak_extra_decimal_scale_from_reward_spread() {
        let engine = DecisionEngine::new(decision_config(QuoteMode::Inside, 1));
        let mut state = test_state();
        state.account.token_balances.clear();
        state.reward = active_reward_state(Utc::now(), dec!(0.055));
        state.books.get_mut("asset-yes").unwrap().bids = vec![BookLevel {
            price: dec!(0.76),
            size: dec!(100),
        }];
        state.books.get_mut("asset-yes").unwrap().asks = vec![BookLevel {
            price: dec!(0.77),
            size: dec!(100),
        }];

        let outcome = engine.evaluate(&state);

        assert_eq!(outcome.desired_quotes.len(), 1);
        assert_eq!(outcome.desired_quotes[0].side, crate::types::QuoteSide::Buy);
        assert_eq!(outcome.desired_quotes[0].price.to_string(), "0.71");
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

        assert_eq!(outcome.desired_quotes.len(), 1);
        assert_eq!(
            outcome.desired_quotes[0].side,
            crate::types::QuoteSide::Sell
        );
        assert_eq!(outcome.desired_quotes[0].price, dec!(0.29));
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

    #[test]
    fn reward_aware_mode_requires_fresh_reward_state() {
        let engine = DecisionEngine::new(decision_config(QuoteMode::Inside, 1));
        let mut state = test_state();
        state.reward.last_success_at = Some(Utc::now() - chrono::Duration::seconds(31));

        let outcome = engine.evaluate(&state);

        assert!(outcome.desired_quotes.is_empty());
        assert_eq!(outcome.reason, "reward state inactive");
    }

    #[test]
    fn reward_aware_mode_quotes_outermost_qualifying_bid_when_only_buy_side_is_available() {
        let engine = DecisionEngine::new(decision_config(QuoteMode::Inside, 1));
        let mut state = test_state();
        state.account.token_balances.clear();
        state.books.get_mut("asset-yes").unwrap().bids = vec![BookLevel {
            price: dec!(0.50),
            size: dec!(100),
        }];
        state.books.get_mut("asset-yes").unwrap().asks = vec![BookLevel {
            price: dec!(0.51),
            size: dec!(100),
        }];

        let outcome = engine.evaluate(&state);

        assert_eq!(outcome.desired_quotes.len(), 1);
        assert_eq!(outcome.desired_quotes[0].side, crate::types::QuoteSide::Buy);
        assert_eq!(outcome.desired_quotes[0].price, dec!(0.48));
    }

    #[test]
    fn reward_aware_mode_prefers_inventory_reducing_sell_when_buy_and_sell_are_equally_safe() {
        let engine = DecisionEngine::new(decision_config(QuoteMode::Inside, 1));
        let mut state = test_state();
        state.books.get_mut("asset-yes").unwrap().bids = vec![BookLevel {
            price: dec!(0.50),
            size: dec!(100),
        }];
        state.books.get_mut("asset-yes").unwrap().asks = vec![BookLevel {
            price: dec!(0.51),
            size: dec!(100),
        }];

        let outcome = engine.evaluate(&state);

        assert_eq!(outcome.desired_quotes.len(), 1);
        assert_eq!(
            outcome.desired_quotes[0].side,
            crate::types::QuoteSide::Sell
        );
        assert_eq!(outcome.desired_quotes[0].price, dec!(0.53));
    }

    #[test]
    fn reward_aware_mode_refuses_to_quote_without_inside_protection() {
        let engine = DecisionEngine::new(decision_config(QuoteMode::Inside, 1));
        let mut state = test_state();
        state.account.token_balances.clear();
        state.reward = active_reward_state(Utc::now(), dec!(0.01));
        state.books.get_mut("asset-yes").unwrap().bids = vec![BookLevel {
            price: dec!(0.50),
            size: dec!(100),
        }];
        state.books.get_mut("asset-yes").unwrap().asks = vec![BookLevel {
            price: dec!(0.51),
            size: dec!(100),
        }];

        let outcome = engine.evaluate(&state);

        assert!(outcome.desired_quotes.is_empty());
        assert_eq!(outcome.reason, "no reward-qualified quotes");
    }
}
