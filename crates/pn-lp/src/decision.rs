use std::str::FromStr;

use chrono::Utc;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::Serialize;

use crate::types::{BookLevel, BookSnapshot, QuoteIntent, QuoteSide, RewardSnapshot, RuntimeState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
    pub diagnostics: DecisionDiagnostics,
}

pub struct DecisionEngine {
    config: DecisionConfig,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DecisionDiagnostics {
    pub quote_mode: QuoteMode,
    pub quote_size: Decimal,
    pub min_spread: Decimal,
    pub min_depth: Decimal,
    pub quote_offset_ticks: u32,
    pub min_usdc_balance: Decimal,
    pub min_token_balance: Decimal,
    pub min_inside_ticks: u32,
    pub min_inside_depth_multiple: Decimal,
    pub min_inside_depth: Decimal,
    pub reward_stale_after_secs: i64,
    pub paused: bool,
    pub flattening: bool,
    pub heartbeat_healthy: bool,
    pub market_feed_healthy: bool,
    pub user_feed_healthy: bool,
    pub signals_allow_quoting: bool,
    pub signals: Vec<SignalDecisionDiagnostics>,
    pub usdc_balance: Decimal,
    pub open_orders: usize,
    pub reward_snapshot_present: bool,
    pub reward_active: bool,
    pub reward_condition_matches: Option<bool>,
    pub reward_max_spread: Option<Decimal>,
    pub reward_min_size: Option<Decimal>,
    pub reward_total_daily_rate: Option<Decimal>,
    pub reward_last_attempt_at: Option<chrono::DateTime<Utc>>,
    pub reward_last_success_at: Option<chrono::DateTime<Utc>>,
    pub reward_active_until: Option<chrono::NaiveDate>,
    pub reward_last_error: Option<String>,
    pub tokens: Vec<TokenDecisionDiagnostics>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SignalDecisionDiagnostics {
    pub name: String,
    pub active: bool,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TokenDecisionDiagnostics {
    pub asset_id: String,
    pub outcome: String,
    pub tick_size: Decimal,
    pub book_present: bool,
    pub best_bid: Option<Decimal>,
    pub best_bid_size: Option<Decimal>,
    pub best_ask: Option<Decimal>,
    pub best_ask_size: Option<Decimal>,
    pub spread: Option<Decimal>,
    pub min_top_depth: Decimal,
    pub adjusted_midpoint: Option<Decimal>,
    pub reward_max_spread: Option<Decimal>,
    pub reward_min_size: Option<Decimal>,
    pub rejection_reason: Option<String>,
    pub buy: Option<SideDecisionDiagnostics>,
    pub sell: Option<SideDecisionDiagnostics>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SideDecisionDiagnostics {
    pub eligible_by_balance: bool,
    pub balance_available: Decimal,
    pub balance_required: Decimal,
    pub qualifying_price: Option<Decimal>,
    pub quote_price: Option<Decimal>,
    pub inside_ticks: Option<u32>,
    pub inside_depth: Option<Decimal>,
    pub selected: bool,
    pub rejection_reason: Option<String>,
}

#[derive(Debug, Clone)]
struct CandidateQuote {
    quote: QuoteIntent,
    inside_ticks: u32,
    inside_depth: Decimal,
    inventory_bias: Decimal,
}

struct TokenEvaluation {
    diagnostics: TokenDecisionDiagnostics,
    candidates: Vec<CandidateQuote>,
}

struct SideEvaluation {
    diagnostics: SideDecisionDiagnostics,
    candidate: Option<CandidateQuote>,
}

impl DecisionEngine {
    pub fn new(config: DecisionConfig) -> Self {
        Self { config }
    }

    pub fn evaluate(&self, state: &RuntimeState) -> DecisionOutcome {
        let now = Utc::now();
        let reward = state.reward.active_snapshot(
            now,
            self.config.reward_stale_after,
            &state.market.condition_id,
        );
        let mut diagnostics = self.base_diagnostics(state, reward);

        if state.flags.paused || state.flags.flattening {
            return DecisionOutcome {
                desired_quotes: Vec::new(),
                cancel_all: true,
                reason: "quoting paused".to_string(),
                diagnostics,
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
                diagnostics,
            };
        }

        if !state.active_signals_allow_quoting() {
            return DecisionOutcome {
                desired_quotes: Vec::new(),
                cancel_all: true,
                reason: "signal gate blocked quoting".to_string(),
                diagnostics,
            };
        }

        let Some(reward) = reward else {
            let cancel_all = !state.open_orders.is_empty();
            return DecisionOutcome {
                desired_quotes: Vec::new(),
                cancel_all,
                reason: "reward state inactive".to_string(),
                diagnostics,
            };
        };

        let mut candidates = Vec::new();
        for token in &state.market.tokens {
            let evaluation = self.reward_candidates(state, token, reward);
            diagnostics.tokens.push(evaluation.diagnostics);
            candidates.extend(evaluation.candidates);
        }

        let desired_quote = candidates
            .into_iter()
            .max_by(|left, right| compare_candidates(left, right))
            .map(|candidate| candidate.quote);
        if let Some(ref quote) = desired_quote {
            diagnostics.mark_selected(quote);
        }
        let desired_quotes = desired_quote.into_iter().collect::<Vec<_>>();
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
            diagnostics,
        }
    }

    fn base_diagnostics(
        &self,
        state: &RuntimeState,
        reward: Option<&RewardSnapshot>,
    ) -> DecisionDiagnostics {
        let mut signals = state
            .signals
            .iter()
            .map(|(name, signal)| SignalDecisionDiagnostics {
                name: name.clone(),
                active: signal.active,
                reason: signal.reason.clone(),
            })
            .collect::<Vec<_>>();
        signals.sort_by(|left, right| left.name.cmp(&right.name));

        let snapshot = state.reward.snapshot.as_ref();

        DecisionDiagnostics {
            quote_mode: self.config.quote_mode,
            quote_size: self.config.quote_size,
            min_spread: self.config.min_spread,
            min_depth: self.config.min_depth,
            quote_offset_ticks: self.config.quote_offset_ticks,
            min_usdc_balance: self.config.min_usdc_balance,
            min_token_balance: self.config.min_token_balance,
            min_inside_ticks: self.config.min_inside_ticks,
            min_inside_depth_multiple: self.config.min_inside_depth_multiple,
            min_inside_depth: self.config.quote_size * self.config.min_inside_depth_multiple,
            reward_stale_after_secs: self.config.reward_stale_after.num_seconds(),
            paused: state.flags.paused,
            flattening: state.flags.flattening,
            heartbeat_healthy: state.flags.heartbeat_healthy,
            market_feed_healthy: state.flags.market_feed_healthy,
            user_feed_healthy: state.flags.user_feed_healthy,
            signals_allow_quoting: state.active_signals_allow_quoting(),
            signals,
            usdc_balance: state.account.usdc_balance,
            open_orders: state.open_orders.len(),
            reward_snapshot_present: snapshot.is_some(),
            reward_active: reward.is_some(),
            reward_condition_matches: snapshot
                .map(|snapshot| snapshot.condition_id == state.market.condition_id),
            reward_max_spread: snapshot.map(|snapshot| snapshot.max_spread),
            reward_min_size: snapshot.map(|snapshot| snapshot.min_size),
            reward_total_daily_rate: snapshot.map(|snapshot| snapshot.total_daily_rate),
            reward_last_attempt_at: state.reward.last_attempt_at,
            reward_last_success_at: state.reward.last_success_at,
            reward_active_until: snapshot.map(|snapshot| snapshot.active_until),
            reward_last_error: state.reward.last_error.clone(),
            tokens: Vec::new(),
        }
    }

    fn reward_candidates(
        &self,
        state: &RuntimeState,
        token: &crate::types::TokenMetadata,
        reward: &RewardSnapshot,
    ) -> TokenEvaluation {
        let mut diagnostics = TokenDecisionDiagnostics {
            asset_id: token.asset_id.clone(),
            outcome: token.outcome.clone(),
            tick_size: token.tick_size,
            book_present: false,
            best_bid: None,
            best_bid_size: None,
            best_ask: None,
            best_ask_size: None,
            spread: None,
            min_top_depth: Decimal::ZERO,
            adjusted_midpoint: None,
            reward_max_spread: Some(reward.max_spread),
            reward_min_size: Some(reward.min_size),
            rejection_reason: None,
            buy: None,
            sell: None,
        };
        let Some(book) = state.books.get(&token.asset_id) else {
            diagnostics.rejection_reason = Some("missing_book".to_string());
            return TokenEvaluation {
                diagnostics,
                candidates: Vec::new(),
            };
        };
        diagnostics.book_present = true;
        let (Some(best_bid), Some(best_ask)) = (book.best_bid(), book.best_ask()) else {
            diagnostics.rejection_reason = Some("missing_best_levels".to_string());
            return TokenEvaluation {
                diagnostics,
                candidates: Vec::new(),
            };
        };
        diagnostics.best_bid = Some(best_bid.price);
        diagnostics.best_bid_size = Some(best_bid.size);
        diagnostics.best_ask = Some(best_ask.price);
        diagnostics.best_ask_size = Some(best_ask.size);

        let spread = best_ask.price - best_bid.price;
        diagnostics.spread = Some(spread);
        diagnostics.min_top_depth = book.min_top_depth();
        if spread < self.config.min_spread {
            diagnostics.rejection_reason = Some("spread_below_min".to_string());
            return TokenEvaluation {
                diagnostics,
                candidates: Vec::new(),
            };
        }
        if diagnostics.min_top_depth < self.config.min_depth {
            diagnostics.rejection_reason = Some("top_depth_below_min".to_string());
            return TokenEvaluation {
                diagnostics,
                candidates: Vec::new(),
            };
        }

        let Some(adjusted_midpoint) = adjusted_midpoint(book, reward.min_size) else {
            diagnostics.rejection_reason = Some("reward_depth_unavailable".to_string());
            return TokenEvaluation {
                diagnostics,
                candidates: Vec::new(),
            };
        };
        diagnostics.adjusted_midpoint = Some(adjusted_midpoint);
        if adjusted_midpoint < Decimal::new(10, 2) || adjusted_midpoint > Decimal::new(90, 2) {
            diagnostics.rejection_reason = Some("adjusted_midpoint_out_of_range".to_string());
            return TokenEvaluation {
                diagnostics,
                candidates: Vec::new(),
            };
        }

        let mut candidates = Vec::new();
        let buy = self.evaluate_side(
            state,
            token.asset_id.as_str(),
            book,
            QuoteSide::Buy,
            adjusted_midpoint,
            reward.max_spread,
            token.tick_size,
        );
        diagnostics.buy = Some(buy.diagnostics);
        if let Some(candidate) = buy.candidate {
            candidates.push(candidate);
        }

        let sell = self.evaluate_side(
            state,
            token.asset_id.as_str(),
            book,
            QuoteSide::Sell,
            adjusted_midpoint,
            reward.max_spread,
            token.tick_size,
        );
        diagnostics.sell = Some(sell.diagnostics);
        if let Some(candidate) = sell.candidate {
            candidates.push(candidate);
        }

        if candidates.is_empty() {
            diagnostics.rejection_reason = Some("no_side_qualified".to_string());
        }

        TokenEvaluation {
            diagnostics,
            candidates,
        }
    }

    fn evaluate_side(
        &self,
        state: &RuntimeState,
        asset_id: &str,
        book: &BookSnapshot,
        side: QuoteSide,
        adjusted_midpoint: Decimal,
        reward_max_spread: Decimal,
        tick_size: Decimal,
    ) -> SideEvaluation {
        let (balance_available, balance_required, qualifying_price, rejection_reason) = match side {
            QuoteSide::Buy => (
                state.account.usdc_balance,
                self.config.min_usdc_balance,
                qualifying_bid_price(adjusted_midpoint, reward_max_spread, tick_size),
                "insufficient_usdc_balance",
            ),
            QuoteSide::Sell => (
                state.account.token_balance(asset_id),
                self.config.min_token_balance,
                qualifying_ask_price(adjusted_midpoint, reward_max_spread, tick_size),
                "insufficient_token_balance",
            ),
        };

        let mut diagnostics = SideDecisionDiagnostics {
            eligible_by_balance: balance_available >= balance_required,
            balance_available,
            balance_required,
            qualifying_price: None,
            quote_price: None,
            inside_ticks: None,
            inside_depth: None,
            selected: false,
            rejection_reason: None,
        };

        if !diagnostics.eligible_by_balance {
            diagnostics.rejection_reason = Some(rejection_reason.to_string());
            return SideEvaluation {
                diagnostics,
                candidate: None,
            };
        }

        let Some(price) = qualifying_price else {
            diagnostics.rejection_reason = Some("qualifying_price_unavailable".to_string());
            return SideEvaluation {
                diagnostics,
                candidate: None,
            };
        };
        diagnostics.qualifying_price = Some(price);
        let quote_price = reward_quote_price(price, book, side.clone(), tick_size, &self.config);
        diagnostics.quote_price = Some(quote_price);

        let inside_ticks = inside_ticks(book, side.clone(), quote_price, tick_size);
        let inside_depth = inside_depth(book, side.clone(), quote_price);
        diagnostics.inside_ticks = Some(inside_ticks);
        diagnostics.inside_depth = Some(inside_depth);
        if inside_ticks < self.config.min_inside_ticks {
            diagnostics.rejection_reason = Some("inside_ticks_below_min".to_string());
            return SideEvaluation {
                diagnostics,
                candidate: None,
            };
        }
        if inside_depth < self.config.quote_size * self.config.min_inside_depth_multiple {
            diagnostics.rejection_reason = Some("inside_depth_below_min".to_string());
            return SideEvaluation {
                diagnostics,
                candidate: None,
            };
        }

        let inventory_bias = match side {
            QuoteSide::Sell => state.account.token_balance(asset_id),
            QuoteSide::Buy => Decimal::ZERO,
        };

        SideEvaluation {
            diagnostics,
            candidate: Some(CandidateQuote {
                quote: QuoteIntent {
                    asset_id: asset_id.to_string(),
                    side,
                    price: quote_price,
                    size: self.config.quote_size,
                    reason: "reward-aware quote".to_string(),
                },
                inside_ticks,
                inside_depth,
                inventory_bias,
            }),
        }
    }
}

impl DecisionDiagnostics {
    fn mark_selected(&mut self, quote: &QuoteIntent) {
        if let Some(token) = self
            .tokens
            .iter_mut()
            .find(|token| token.asset_id == quote.asset_id)
        {
            let side = match quote.side {
                QuoteSide::Buy => &mut token.buy,
                QuoteSide::Sell => &mut token.sell,
            };
            if let Some(side) = side.as_mut() {
                side.selected = true;
            }
        }
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

fn reward_quote_price(
    qualifying_price: Decimal,
    book: &BookSnapshot,
    side: QuoteSide,
    tick_size: Decimal,
    config: &DecisionConfig,
) -> Decimal {
    if config.quote_offset_ticks == 0 {
        return qualifying_price;
    }

    let tick_offset = tick_size * Decimal::from(config.quote_offset_ticks);
    let inset_price = match side {
        QuoteSide::Buy => clamp_reward_quote_price(qualifying_price + tick_offset, tick_size),
        QuoteSide::Sell => clamp_reward_quote_price(qualifying_price - tick_offset, tick_size),
    };

    if preserves_inside_ticks(book, side, inset_price, tick_size, config.min_inside_ticks) {
        inset_price
    } else {
        qualifying_price
    }
}

fn preserves_inside_ticks(
    book: &BookSnapshot,
    side: QuoteSide,
    price: Decimal,
    tick_size: Decimal,
    min_inside_ticks: u32,
) -> bool {
    if min_inside_ticks == 0 {
        return true;
    }

    inside_ticks(book, side, price, tick_size) >= min_inside_ticks
}

fn clamp_reward_quote_price(price: Decimal, tick_size: Decimal) -> Decimal {
    let mut clamped = price.clamp(tick_size, Decimal::ONE - tick_size);
    clamped.rescale(tick_size.scale());
    clamped
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
            terminal_order_ids: std::collections::HashSet::new(),
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
        assert_eq!(outcome.desired_quotes[0].price, dec!(0.52));
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
        assert_eq!(outcome.desired_quotes[0].price.to_string(), "0.72");
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
        assert_eq!(outcome.desired_quotes[0].price, dec!(0.49));
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
        assert_eq!(outcome.desired_quotes[0].price, dec!(0.52));
    }

    #[test]
    fn reward_aware_mode_insets_one_tick_inside_reward_boundary() {
        let engine = DecisionEngine::new(decision_config(QuoteMode::Outside, 1));
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
        let token = outcome
            .diagnostics
            .tokens
            .iter()
            .find(|token| token.asset_id == "asset-yes")
            .expect("asset diagnostics");
        let sell = token.sell.as_ref().expect("sell diagnostics");

        assert_eq!(outcome.desired_quotes.len(), 1);
        assert_eq!(
            outcome.desired_quotes[0].side,
            crate::types::QuoteSide::Sell
        );
        assert_eq!(outcome.desired_quotes[0].price, dec!(0.52));
        assert_eq!(sell.qualifying_price, Some(dec!(0.53)));
        assert_eq!(sell.quote_price, Some(dec!(0.52)));
        assert_eq!(sell.inside_ticks, Some(1));
        assert_eq!(sell.inside_depth, Some(dec!(100)));
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

    #[test]
    fn reward_aware_mode_exposes_rejection_diagnostics() {
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
        let token = outcome
            .diagnostics
            .tokens
            .iter()
            .find(|token| token.asset_id == "asset-yes")
            .expect("asset diagnostics");

        assert_eq!(outcome.reason, "no reward-qualified quotes");
        assert_eq!(outcome.diagnostics.quote_size, dec!(10));
        assert_eq!(outcome.diagnostics.quote_offset_ticks, 1);
        assert_eq!(outcome.diagnostics.min_inside_ticks, 1);
        assert_eq!(outcome.diagnostics.min_inside_depth, dec!(15));
        assert_eq!(token.best_bid, Some(dec!(0.50)));
        assert_eq!(token.best_ask, Some(dec!(0.51)));
        assert_eq!(token.spread, Some(dec!(0.01)));
        assert_eq!(token.adjusted_midpoint, Some(dec!(0.505)));
        assert_eq!(token.reward_max_spread, Some(dec!(0.01)));
        assert_eq!(token.reward_min_size, Some(dec!(50)));
        let buy = token.buy.as_ref().expect("buy diagnostics");
        assert_eq!(buy.qualifying_price, Some(dec!(0.50)));
        assert_eq!(buy.quote_price, Some(dec!(0.50)));
        assert_eq!(buy.inside_ticks, Some(0));
        assert_eq!(buy.inside_depth, Some(dec!(0)));
        assert_eq!(
            buy.rejection_reason.as_deref(),
            Some("inside_ticks_below_min")
        );
        let sell = token.sell.as_ref().expect("sell diagnostics");
        assert!(!sell.eligible_by_balance);
        assert_eq!(
            sell.rejection_reason.as_deref(),
            Some("insufficient_token_balance")
        );
    }
}
