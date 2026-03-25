use anyhow::{anyhow, Result};
use chrono::Utc;
use pn_polymarket::{GammaMarketSummary, PublicBookLevel, PublicOrderBook};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

const SINGLE_SIDED_FACTOR: u32 = 3;
const DAYS_PER_YEAR: u32 = 365;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalyzerConfig {
    pub quote_size: Decimal,
    pub quote_offset_ticks: u32,
    pub min_inside_ticks: u32,
    pub min_inside_depth_multiple: Decimal,
    pub top_n: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StrategyProfile {
    OuterLowRisk,
    AggressiveMid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyAnalysis {
    pub profile: StrategyProfile,
    pub recommended_side: Side,
    pub recommended_price: Decimal,
    pub qualifying_boundary: Decimal,
    pub inside_ticks: u32,
    pub inside_depth: Decimal,
    pub existing_qualified_ahead_size: Decimal,
    pub reward_gap_headroom: Decimal,
    pub estimated_share: Decimal,
    pub estimated_daily_reward: Decimal,
    pub roi_daily_conservative: Decimal,
    pub roi_annual_conservative: Decimal,
    pub crowdedness_score: Decimal,
    pub our_q_min: Decimal,
    pub our_side_score: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketAnalysis {
    pub condition_id: String,
    pub slug: String,
    pub question: String,
    pub daily_reward: Decimal,
    pub gross_annual_reward: Decimal,
    pub rewards_min_size: Decimal,
    pub rewards_max_spread: Decimal,
    pub midpoint: Decimal,
    pub best_bid: Option<Decimal>,
    pub best_ask: Option<Decimal>,
    pub current_spread: Option<Decimal>,
    pub effective_quote_size: Decimal,
    pub current_config_eligible: bool,
    pub existing_q_min: Decimal,
    pub outer_low_risk: Option<StrategyAnalysis>,
    pub aggressive_mid: Option<StrategyAnalysis>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewardDistribution {
    pub min: Decimal,
    pub median: Decimal,
    pub p90: Decimal,
    pub max: Decimal,
}

pub fn analyze_market(
    market: &GammaMarketSummary,
    book: &PublicOrderBook,
    config: &AnalyzerConfig,
) -> Result<MarketAnalysis> {
    let rewards_min_size = market
        .rewards_min_size
        .ok_or_else(|| anyhow!("market {} missing rewardsMinSize", market.condition_id))?;
    let rewards_max_spread = market
        .rewards_max_spread
        .ok_or_else(|| anyhow!("market {} missing rewardsMaxSpread", market.condition_id))?;
    let max_spread = rewards_max_spread / Decimal::from(100u32);
    if max_spread <= Decimal::ZERO {
        return Err(anyhow!(
            "market {} has non-positive reward spread {}",
            market.condition_id,
            rewards_max_spread
        ));
    }

    let midpoint = adjusted_midpoint(book, rewards_min_size)
        .ok_or_else(|| anyhow!("market {} lacks qualifying depth", market.condition_id))?;
    let daily_reward = active_daily_reward(market);
    let effective_quote_size = config.quote_size.max(rewards_min_size);
    let current_config_eligible = config.quote_size >= rewards_min_size;
    let existing_q_min = existing_book_q_min(book, midpoint, max_spread);
    let best_bid = book
        .bids
        .first()
        .map(|level| level.price)
        .or(market.best_bid);
    let best_ask = book
        .asks
        .first()
        .map(|level| level.price)
        .or(market.best_ask);
    let current_spread = best_bid.zip(best_ask).map(|(bid, ask)| ask - bid);

    Ok(MarketAnalysis {
        condition_id: market.condition_id.clone(),
        slug: market.slug.clone(),
        question: market.question.clone(),
        daily_reward,
        gross_annual_reward: daily_reward * Decimal::from(DAYS_PER_YEAR),
        rewards_min_size,
        rewards_max_spread,
        midpoint,
        best_bid,
        best_ask,
        current_spread,
        effective_quote_size,
        current_config_eligible,
        existing_q_min,
        outer_low_risk: select_outer_low_risk(
            book,
            midpoint,
            max_spread,
            effective_quote_size,
            daily_reward,
            existing_q_min,
            config,
        ),
        aggressive_mid: select_aggressive_mid(
            book,
            midpoint,
            max_spread,
            effective_quote_size,
            daily_reward,
            existing_q_min,
            config,
        ),
    })
}

pub fn render_shortlist_toml(
    generated_at: &str,
    config: &AnalyzerConfig,
    analyses: &[MarketAnalysis],
) -> String {
    let mut outer = analyses
        .iter()
        .filter_map(|analysis| {
            analysis
                .outer_low_risk
                .as_ref()
                .map(|profile| (analysis, profile))
        })
        .collect::<Vec<_>>();
    outer.sort_by(compare_profile_rank);
    outer.truncate(config.top_n);

    let mut aggressive = analyses
        .iter()
        .filter_map(|analysis| {
            analysis
                .aggressive_mid
                .as_ref()
                .map(|profile| (analysis, profile))
        })
        .collect::<Vec<_>>();
    aggressive.sort_by(compare_profile_rank);
    aggressive.truncate(config.top_n);

    let mut output = String::new();
    output.push_str(&format!("generated_at = \"{}\"\n", generated_at));
    output.push_str(&format!("scan_count = {}\n", analyses.len()));
    output.push_str("\n[defaults]\n");
    output.push_str(&format!("quote_size = \"{}\"\n", config.quote_size));
    output.push_str(&format!(
        "quote_offset_ticks = {}\nmin_inside_ticks = {}\nmin_inside_depth_multiple = \"{}\"\ntop_n = {}\n",
        config.quote_offset_ticks,
        config.min_inside_ticks,
        config.min_inside_depth_multiple,
        config.top_n
    ));

    output.push_str("\n[outer_low_risk]\n");
    output.push_str(&format!("count = {}\n", outer.len()));
    for (analysis, profile) in outer {
        render_profile_entry(&mut output, "outer_low_risk", analysis, profile);
    }

    output.push_str("\n[aggressive_mid]\n");
    output.push_str(&format!("count = {}\n", aggressive.len()));
    for (analysis, profile) in aggressive {
        render_profile_entry(&mut output, "aggressive_mid", analysis, profile);
    }

    output
}

pub fn reward_distribution(analyses: &[MarketAnalysis]) -> Option<RewardDistribution> {
    let mut values = analyses
        .iter()
        .map(|analysis| analysis.daily_reward)
        .filter(|value| *value > Decimal::ZERO)
        .collect::<Vec<_>>();
    if values.is_empty() {
        return None;
    }
    values.sort();
    let last = values.len() - 1;

    Some(RewardDistribution {
        min: values[0],
        median: percentile(&values, 0.5),
        p90: percentile(&values, 0.9),
        max: values[last],
    })
}

pub fn active_daily_reward(market: &GammaMarketSummary) -> Decimal {
    let today = Utc::now().date_naive();
    market
        .clob_rewards
        .iter()
        .filter(|reward| reward.daily_rate > Decimal::ZERO)
        .filter(|reward| {
            let starts_ok = reward
                .start_date
                .as_deref()
                .and_then(parse_date)
                .map(|date| date <= today)
                .unwrap_or(true);
            let ends_ok = reward
                .end_date
                .as_deref()
                .and_then(parse_date)
                .map(|date| date >= today)
                .unwrap_or(true);
            starts_ok && ends_ok
        })
        .fold(Decimal::ZERO, |sum, reward| sum + reward.daily_rate)
}

fn render_profile_entry(
    output: &mut String,
    table_name: &str,
    analysis: &MarketAnalysis,
    profile: &StrategyAnalysis,
) {
    output.push_str(&format!("\n[[{}.entries]]\n", table_name));
    output.push_str(&format!("condition_id = \"{}\"\n", analysis.condition_id));
    output.push_str(&format!("slug = \"{}\"\n", analysis.slug));
    output.push_str(&format!(
        "question = \"{}\"\n",
        analysis.question.replace('\\', "\\\\").replace('"', "\\\"")
    ));
    output.push_str(&format!("daily_reward = \"{}\"\n", analysis.daily_reward));
    output.push_str(&format!(
        "gross_annual_reward = \"{}\"\n",
        analysis.gross_annual_reward
    ));
    output.push_str(&format!(
        "rewards_min_size = \"{}\"\nrewards_max_spread = \"{}\"\n",
        analysis.rewards_min_size, analysis.rewards_max_spread
    ));
    output.push_str(&format!("midpoint = \"{}\"\n", analysis.midpoint));
    output.push_str(&format!(
        "best_bid = {}\nbest_ask = {}\ncurrent_spread = {}\n",
        format_optional_decimal(analysis.best_bid),
        format_optional_decimal(analysis.best_ask),
        format_optional_decimal(analysis.current_spread)
    ));
    output.push_str(&format!(
        "recommended_side = \"{}\"\nrecommended_price = \"{}\"\nqualifying_boundary = \"{}\"\n",
        match profile.recommended_side {
            Side::Buy => "buy",
            Side::Sell => "sell",
        },
        profile.recommended_price,
        profile.qualifying_boundary
    ));
    output.push_str(&format!(
        "effective_quote_size = \"{}\"\ncurrent_config_eligible = {}\n",
        analysis.effective_quote_size, analysis.current_config_eligible
    ));
    output.push_str(&format!(
        "estimated_share = \"{}\"\nestimated_daily_reward = \"{}\"\n",
        profile.estimated_share, profile.estimated_daily_reward
    ));
    output.push_str(&format!(
        "roi_daily_conservative = \"{}\"\nroi_annual_conservative = \"{}\"\n",
        profile.roi_daily_conservative, profile.roi_annual_conservative
    ));
    output.push_str(&format!(
        "inside_ticks = {}\ninside_depth = \"{}\"\ncrowdedness_score = \"{}\"\n",
        profile.inside_ticks, profile.inside_depth, profile.crowdedness_score
    ));
}

fn parse_date(value: &str) -> Option<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()
}

fn format_optional_decimal(value: Option<Decimal>) -> String {
    value
        .map(|value| format!("\"{}\"", value))
        .unwrap_or_else(|| "null".to_string())
}

fn percentile(values: &[Decimal], ratio: f64) -> Decimal {
    if values.is_empty() {
        return Decimal::ZERO;
    }
    let max_index = values.len().saturating_sub(1);
    let position = (max_index as f64 * ratio).round() as usize;
    values[position.min(max_index)]
}

fn select_outer_low_risk(
    book: &PublicOrderBook,
    midpoint: Decimal,
    max_spread: Decimal,
    effective_quote_size: Decimal,
    daily_reward: Decimal,
    existing_q_min: Decimal,
    config: &AnalyzerConfig,
) -> Option<StrategyAnalysis> {
    let buy = build_outer_candidate(
        book,
        Side::Buy,
        midpoint,
        max_spread,
        effective_quote_size,
        daily_reward,
        existing_q_min,
        config,
    );
    let sell = build_outer_candidate(
        book,
        Side::Sell,
        midpoint,
        max_spread,
        effective_quote_size,
        daily_reward,
        existing_q_min,
        config,
    );

    match (buy, sell) {
        (Some(left), Some(right)) => {
            if compare_outer_candidates(&left, &right).is_gt() {
                Some(left)
            } else {
                Some(right)
            }
        }
        (Some(candidate), None) | (None, Some(candidate)) => Some(candidate),
        (None, None) => None,
    }
}

fn select_aggressive_mid(
    book: &PublicOrderBook,
    midpoint: Decimal,
    max_spread: Decimal,
    effective_quote_size: Decimal,
    daily_reward: Decimal,
    existing_q_min: Decimal,
    config: &AnalyzerConfig,
) -> Option<StrategyAnalysis> {
    let mut candidates = enumerate_profile_candidates(
        StrategyProfile::AggressiveMid,
        book,
        Side::Buy,
        midpoint,
        max_spread,
        effective_quote_size,
        daily_reward,
        existing_q_min,
        config,
    );
    candidates.extend(enumerate_profile_candidates(
        StrategyProfile::AggressiveMid,
        book,
        Side::Sell,
        midpoint,
        max_spread,
        effective_quote_size,
        daily_reward,
        existing_q_min,
        config,
    ));
    candidates.into_iter().max_by(compare_aggressive_candidates)
}

fn build_outer_candidate(
    book: &PublicOrderBook,
    side: Side,
    midpoint: Decimal,
    max_spread: Decimal,
    effective_quote_size: Decimal,
    daily_reward: Decimal,
    existing_q_min: Decimal,
    config: &AnalyzerConfig,
) -> Option<StrategyAnalysis> {
    let tick_size = book.tick_size;
    let qualifying_boundary = qualifying_boundary(side, midpoint, max_spread, tick_size)?;
    let outermost_scoring_price = next_price_toward_midpoint(side, qualifying_boundary, tick_size);
    let price = reward_quote_price(outermost_scoring_price, book, side, tick_size, config);
    build_candidate(
        StrategyProfile::OuterLowRisk,
        book,
        side,
        price,
        qualifying_boundary,
        midpoint,
        max_spread,
        effective_quote_size,
        daily_reward,
        existing_q_min,
        config,
    )
}

fn enumerate_profile_candidates(
    profile: StrategyProfile,
    book: &PublicOrderBook,
    side: Side,
    midpoint: Decimal,
    max_spread: Decimal,
    effective_quote_size: Decimal,
    daily_reward: Decimal,
    existing_q_min: Decimal,
    config: &AnalyzerConfig,
) -> Vec<StrategyAnalysis> {
    let tick_size = book.tick_size;
    let Some(boundary) = qualifying_boundary(side, midpoint, max_spread, tick_size) else {
        return Vec::new();
    };

    let mut candidates = Vec::new();
    let mut current = boundary;
    loop {
        let Some(candidate) = build_candidate(
            profile,
            book,
            side,
            current,
            boundary,
            midpoint,
            max_spread,
            effective_quote_size,
            daily_reward,
            existing_q_min,
            config,
        ) else {
            current = next_price_toward_midpoint(side, current, tick_size);
            if !price_in_profile_window(side, current, midpoint, tick_size) {
                break;
            }
            continue;
        };
        candidates.push(candidate);
        current = next_price_toward_midpoint(side, current, tick_size);
        if !price_in_profile_window(side, current, midpoint, tick_size) {
            break;
        }
    }

    candidates
}

fn price_in_profile_window(
    side: Side,
    price: Decimal,
    midpoint: Decimal,
    tick_size: Decimal,
) -> bool {
    match side {
        Side::Buy => price >= tick_size && price < midpoint,
        Side::Sell => price > midpoint && price <= Decimal::ONE - tick_size,
    }
}

fn next_price_toward_midpoint(side: Side, price: Decimal, tick_size: Decimal) -> Decimal {
    match side {
        Side::Buy => price + tick_size,
        Side::Sell => price - tick_size,
    }
}

#[allow(clippy::too_many_arguments)]
fn build_candidate(
    profile: StrategyProfile,
    book: &PublicOrderBook,
    side: Side,
    price: Decimal,
    qualifying_boundary: Decimal,
    midpoint: Decimal,
    max_spread: Decimal,
    effective_quote_size: Decimal,
    daily_reward: Decimal,
    existing_q_min: Decimal,
    config: &AnalyzerConfig,
) -> Option<StrategyAnalysis> {
    let spread_from_mid = match side {
        Side::Buy => midpoint - price,
        Side::Sell => price - midpoint,
    };
    if spread_from_mid < Decimal::ZERO || spread_from_mid > max_spread {
        return None;
    }

    let inside_ticks = inside_ticks(book, side, price, book.tick_size);
    if inside_ticks < config.min_inside_ticks {
        return None;
    }

    let inside_depth = inside_depth(book, side, price);
    let min_inside_depth = effective_quote_size * config.min_inside_depth_multiple;
    if inside_depth < min_inside_depth {
        return None;
    }

    let our_side_score = reward_score(max_spread, spread_from_mid, effective_quote_size);
    let our_q_min = match side {
        Side::Buy => q_min(
            our_side_score,
            Decimal::ZERO,
            midpoint,
            Decimal::from(SINGLE_SIDED_FACTOR),
        ),
        Side::Sell => q_min(
            Decimal::ZERO,
            our_side_score,
            midpoint,
            Decimal::from(SINGLE_SIDED_FACTOR),
        ),
    };
    if our_q_min <= Decimal::ZERO {
        return None;
    }

    let denominator = existing_q_min + our_q_min;
    let estimated_share = if denominator > Decimal::ZERO {
        our_q_min / denominator
    } else {
        Decimal::ONE
    };
    let estimated_daily_reward = daily_reward * estimated_share;
    let roi_daily_conservative = if effective_quote_size > Decimal::ZERO {
        estimated_daily_reward / effective_quote_size
    } else {
        Decimal::ZERO
    };
    let existing_qualified_ahead_size =
        qualified_ahead_size(book, side, price, midpoint, max_spread);
    let crowdedness_score = if effective_quote_size > Decimal::ZERO {
        existing_qualified_ahead_size / effective_quote_size
    } else {
        Decimal::ZERO
    };

    Some(StrategyAnalysis {
        profile,
        recommended_side: side,
        recommended_price: price,
        qualifying_boundary,
        inside_ticks,
        inside_depth,
        existing_qualified_ahead_size,
        reward_gap_headroom: max_spread - spread_from_mid,
        estimated_share,
        estimated_daily_reward,
        roi_daily_conservative,
        roi_annual_conservative: roi_daily_conservative * Decimal::from(DAYS_PER_YEAR),
        crowdedness_score,
        our_q_min,
        our_side_score,
    })
}

fn compare_outer_candidates(
    left: &StrategyAnalysis,
    right: &StrategyAnalysis,
) -> std::cmp::Ordering {
    left.inside_ticks
        .cmp(&right.inside_ticks)
        .then_with(|| left.inside_depth.cmp(&right.inside_depth))
        .then_with(|| left.reward_gap_headroom.cmp(&right.reward_gap_headroom))
        .then_with(|| right.crowdedness_score.cmp(&left.crowdedness_score))
        .then_with(|| left.recommended_price.cmp(&right.recommended_price))
}

fn compare_aggressive_candidates(
    left: &StrategyAnalysis,
    right: &StrategyAnalysis,
) -> std::cmp::Ordering {
    left.roi_daily_conservative
        .cmp(&right.roi_daily_conservative)
        .then_with(|| {
            left.estimated_daily_reward
                .cmp(&right.estimated_daily_reward)
        })
        .then_with(|| right.crowdedness_score.cmp(&left.crowdedness_score))
        .then_with(|| left.inside_ticks.cmp(&right.inside_ticks))
}

fn compare_profile_rank(
    left: &(&MarketAnalysis, &StrategyAnalysis),
    right: &(&MarketAnalysis, &StrategyAnalysis),
) -> std::cmp::Ordering {
    let left_profile = left.1;
    let right_profile = right.1;
    left_profile
        .roi_daily_conservative
        .cmp(&right_profile.roi_daily_conservative)
        .then_with(|| {
            left_profile
                .estimated_daily_reward
                .cmp(&right_profile.estimated_daily_reward)
        })
        .then_with(|| {
            right_profile
                .crowdedness_score
                .cmp(&left_profile.crowdedness_score)
        })
}

fn existing_book_q_min(book: &PublicOrderBook, midpoint: Decimal, max_spread: Decimal) -> Decimal {
    let q_one = book
        .bids
        .iter()
        .filter_map(|level| level_score(level, Side::Buy, midpoint, max_spread))
        .fold(Decimal::ZERO, |sum, score| sum + score);
    let q_two = book
        .asks
        .iter()
        .filter_map(|level| level_score(level, Side::Sell, midpoint, max_spread))
        .fold(Decimal::ZERO, |sum, score| sum + score);

    q_min(q_one, q_two, midpoint, Decimal::from(SINGLE_SIDED_FACTOR))
}

fn level_score(
    level: &PublicBookLevel,
    side: Side,
    midpoint: Decimal,
    max_spread: Decimal,
) -> Option<Decimal> {
    let spread_from_mid = match side {
        Side::Buy => midpoint - level.price,
        Side::Sell => level.price - midpoint,
    };
    if spread_from_mid < Decimal::ZERO || spread_from_mid > max_spread {
        return None;
    }
    Some(reward_score(max_spread, spread_from_mid, level.size))
}

fn reward_score(max_spread: Decimal, spread_from_mid: Decimal, size: Decimal) -> Decimal {
    if size <= Decimal::ZERO || max_spread <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    if spread_from_mid < Decimal::ZERO || spread_from_mid > max_spread {
        return Decimal::ZERO;
    }

    let decay = (max_spread - spread_from_mid) / max_spread;
    decay * decay * size
}

fn q_min(
    q_one: Decimal,
    q_two: Decimal,
    midpoint: Decimal,
    single_sided_factor: Decimal,
) -> Decimal {
    if midpoint >= Decimal::new(10, 2) && midpoint <= Decimal::new(90, 2) {
        q_one
            .min(q_two)
            .max(q_one / single_sided_factor)
            .max(q_two / single_sided_factor)
    } else {
        q_one.min(q_two)
    }
}

fn adjusted_midpoint(book: &PublicOrderBook, min_size: Decimal) -> Option<Decimal> {
    let bid = price_at_cumulative_depth(&book.bids, min_size, true)?;
    let ask = price_at_cumulative_depth(&book.asks, min_size, false)?;
    Some((bid + ask) / Decimal::from(2u32))
}

fn price_at_cumulative_depth(
    levels: &[PublicBookLevel],
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

fn qualifying_boundary(
    side: Side,
    midpoint: Decimal,
    max_spread: Decimal,
    tick_size: Decimal,
) -> Option<Decimal> {
    match side {
        Side::Buy => {
            let price = midpoint - max_spread;
            let aligned = round_up_to_tick(price.max(tick_size), tick_size);
            (aligned < Decimal::ONE).then_some(aligned)
        }
        Side::Sell => {
            let price = midpoint + max_spread;
            let aligned = round_down_to_tick(price.min(Decimal::ONE - tick_size), tick_size);
            (aligned > Decimal::ZERO).then_some(aligned)
        }
    }
}

fn reward_quote_price(
    qualifying_price: Decimal,
    book: &PublicOrderBook,
    side: Side,
    tick_size: Decimal,
    config: &AnalyzerConfig,
) -> Decimal {
    if config.quote_offset_ticks == 0 {
        return qualifying_price;
    }

    let tick_offset = tick_size * Decimal::from(config.quote_offset_ticks);
    let inset_price = match side {
        Side::Buy => clamp_reward_quote_price(qualifying_price + tick_offset, tick_size),
        Side::Sell => clamp_reward_quote_price(qualifying_price - tick_offset, tick_size),
    };

    if inside_ticks(book, side, inset_price, tick_size) >= config.min_inside_ticks {
        inset_price
    } else {
        qualifying_price
    }
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

fn inside_ticks(book: &PublicOrderBook, side: Side, price: Decimal, tick_size: Decimal) -> u32 {
    let distance = match side {
        Side::Buy => book
            .bids
            .iter()
            .filter(|level| level.price > price)
            .map(|level| level.price - price)
            .max(),
        Side::Sell => book
            .asks
            .iter()
            .filter(|level| level.price < price)
            .map(|level| price - level.price)
            .max(),
    };

    distance
        .and_then(|value| (value / tick_size).trunc().to_u32())
        .unwrap_or(0)
}

fn inside_depth(book: &PublicOrderBook, side: Side, price: Decimal) -> Decimal {
    match side {
        Side::Buy => book
            .bids
            .iter()
            .filter(|level| level.price > price)
            .fold(Decimal::ZERO, |sum, level| sum + level.size),
        Side::Sell => book
            .asks
            .iter()
            .filter(|level| level.price < price)
            .fold(Decimal::ZERO, |sum, level| sum + level.size),
    }
}

fn qualified_ahead_size(
    book: &PublicOrderBook,
    side: Side,
    price: Decimal,
    midpoint: Decimal,
    max_spread: Decimal,
) -> Decimal {
    match side {
        Side::Buy => book
            .bids
            .iter()
            .filter(|level| level.price > price)
            .filter(|level| midpoint - level.price <= max_spread)
            .fold(Decimal::ZERO, |sum, level| sum + level.size),
        Side::Sell => book
            .asks
            .iter()
            .filter(|level| level.price < price)
            .filter(|level| level.price - midpoint <= max_spread)
            .fold(Decimal::ZERO, |sum, level| sum + level.size),
    }
}

#[cfg(test)]
mod tests {
    use pn_polymarket::{GammaClobReward, GammaMarketSummary, GammaToken, PublicBookLevel};
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    use super::{
        analyze_market, q_min, render_shortlist_toml, AnalyzerConfig, Side, StrategyProfile,
    };

    #[test]
    fn midpoint_range_single_sided_q_min_is_discounted_by_c() {
        assert_eq!(q_min(dec!(9), dec!(0), dec!(0.50), dec!(3)), dec!(3));
        assert_eq!(q_min(dec!(0), dec!(12), dec!(0.50), dec!(3)), dec!(4));
    }

    #[test]
    fn extreme_midpoint_single_sided_q_min_is_zero() {
        assert_eq!(q_min(dec!(9), dec!(0), dec!(0.95), dec!(3)), dec!(0));
        assert_eq!(q_min(dec!(0), dec!(12), dec!(0.05), dec!(3)), dec!(0));
    }

    #[test]
    fn analyze_market_flags_config_below_reward_min_size_and_prefers_outer_vs_aggressive() {
        let config = AnalyzerConfig {
            quote_size: dec!(10),
            quote_offset_ticks: 0,
            min_inside_ticks: 1,
            min_inside_depth_multiple: dec!(1.5),
            top_n: 10,
        };
        let market = sample_market();
        let book = sample_book();

        let analysis = analyze_market(&market, &book, &config).expect("analysis");

        assert!(!analysis.current_config_eligible);
        assert_eq!(analysis.effective_quote_size, dec!(20));
        assert!(analysis.existing_q_min > Decimal::ZERO);

        let outer = analysis.outer_low_risk.expect("outer profile");
        assert_eq!(outer.profile, StrategyProfile::OuterLowRisk);
        assert_eq!(outer.recommended_side, Side::Buy);
        assert_eq!(outer.recommended_price, dec!(0.466));
        assert_eq!(outer.qualifying_boundary, dec!(0.465));

        let aggressive = analysis.aggressive_mid.expect("aggressive profile");
        assert_eq!(aggressive.profile, StrategyProfile::AggressiveMid);
        assert_eq!(aggressive.recommended_side, Side::Buy);
        assert!(aggressive.recommended_price > outer.recommended_price);
        assert!(aggressive.roi_daily_conservative > outer.roi_daily_conservative);
    }

    #[test]
    fn render_shortlist_toml_emits_metadata_and_profile_sections() {
        let config = AnalyzerConfig {
            quote_size: dec!(10),
            quote_offset_ticks: 0,
            min_inside_ticks: 1,
            min_inside_depth_multiple: dec!(1.5),
            top_n: 5,
        };
        let analysis = analyze_market(&sample_market(), &sample_book(), &config).expect("analysis");
        let rendered = render_shortlist_toml("2026-03-25T10:00:00Z", &config, &[analysis]);

        assert!(rendered.contains("generated_at = \"2026-03-25T10:00:00Z\""));
        assert!(rendered.contains("[outer_low_risk]"));
        assert!(rendered.contains("[aggressive_mid]"));
        assert!(rendered.contains("condition_id = \"condition-1\""));
        assert!(rendered.contains("recommended_price = \"0.466\""));
    }

    fn sample_market() -> GammaMarketSummary {
        GammaMarketSummary {
            condition_id: "condition-1".to_string(),
            question: "Will X happen?".to_string(),
            slug: "will-x-happen".to_string(),
            tokens: vec![
                GammaToken {
                    token_id: "yes-token".to_string(),
                    outcome: "Yes".to_string(),
                },
                GammaToken {
                    token_id: "no-token".to_string(),
                    outcome: "No".to_string(),
                },
            ],
            active: true,
            closed: false,
            best_bid: Some(dec!(0.490)),
            best_ask: Some(dec!(0.510)),
            spread: Some(dec!(0.020)),
            liquidity_clob: None,
            volume_24hr_clob: None,
            competitive: Some(dec!(0.75)),
            rewards_min_size: Some(dec!(20)),
            rewards_max_spread: Some(dec!(3.5)),
            clob_rewards: vec![GammaClobReward {
                id: "reward-1".to_string(),
                condition_id: "condition-1".to_string(),
                daily_rate: dec!(50),
                start_date: Some("2026-03-01".to_string()),
                end_date: Some("2500-12-31".to_string()),
            }],
        }
    }

    fn sample_book() -> pn_polymarket::PublicOrderBook {
        pn_polymarket::PublicOrderBook {
            market: "condition-1".to_string(),
            asset_id: "yes-token".to_string(),
            timestamp: "1".to_string(),
            hash: "hash".to_string(),
            bids: vec![
                PublicBookLevel {
                    price: dec!(0.490),
                    size: dec!(100),
                },
                PublicBookLevel {
                    price: dec!(0.489),
                    size: dec!(40),
                },
                PublicBookLevel {
                    price: dec!(0.480),
                    size: dec!(20),
                },
            ],
            asks: vec![
                PublicBookLevel {
                    price: dec!(0.510),
                    size: dec!(20),
                },
                PublicBookLevel {
                    price: dec!(0.520),
                    size: dec!(15),
                },
                PublicBookLevel {
                    price: dec!(0.535),
                    size: dec!(20),
                },
            ],
            min_order_size: dec!(5),
            tick_size: dec!(0.001),
            neg_risk: false,
            last_trade_price: Some(dec!(0.500)),
        }
    }
}
