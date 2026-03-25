use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::rewards_analyzer::{MarketAnalysis, StrategyAnalysis};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileSelection {
    OuterLowRisk,
    AggressiveMid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortKey {
    RoiDaily,
    RoiAnnual,
    EstimatedDailyReward,
    DailyReward,
    Crowdedness,
    Safety,
    Balanced,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterConfig {
    pub profile: ProfileSelection,
    pub sort_by: SortKey,
    pub top_n: usize,
    pub min_roi_daily: Option<Decimal>,
    pub min_daily_reward: Option<Decimal>,
    pub max_crowdedness: Option<Decimal>,
    pub min_inside_ticks: Option<u32>,
    pub eligible_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilteredMarket {
    pub market: MarketAnalysis,
    pub strategy: StrategyAnalysis,
    pub safety_score: Decimal,
    pub risk_score: Decimal,
    pub balanced_score: Decimal,
    pub inside_depth_multiple: Decimal,
    pub headroom_ratio: Decimal,
}

pub fn filter_markets(analyses: &[MarketAnalysis], config: &FilterConfig) -> Vec<FilteredMarket> {
    let mut filtered = analyses
        .iter()
        .filter_map(|analysis| {
            select_profile(analysis, config.profile).map(|strategy| (analysis, strategy))
        })
        .filter(|(analysis, strategy)| matches_thresholds(analysis, strategy, config))
        .map(|(analysis, strategy)| build_filtered_market(analysis, strategy))
        .collect::<Vec<_>>();

    filtered.sort_by(|left, right| compare_entries(left, right, config.sort_by));
    filtered.truncate(config.top_n);
    filtered
}

fn select_profile(
    analysis: &MarketAnalysis,
    profile: ProfileSelection,
) -> Option<&StrategyAnalysis> {
    match profile {
        ProfileSelection::OuterLowRisk => analysis.outer_low_risk.as_ref(),
        ProfileSelection::AggressiveMid => analysis.aggressive_mid.as_ref(),
    }
}

fn matches_thresholds(
    analysis: &MarketAnalysis,
    strategy: &StrategyAnalysis,
    config: &FilterConfig,
) -> bool {
    if config.eligible_only && !analysis.current_config_eligible {
        return false;
    }
    if let Some(min_roi_daily) = config.min_roi_daily {
        if strategy.roi_daily_conservative < min_roi_daily {
            return false;
        }
    }
    if let Some(min_daily_reward) = config.min_daily_reward {
        if analysis.daily_reward < min_daily_reward {
            return false;
        }
    }
    if let Some(max_crowdedness) = config.max_crowdedness {
        if strategy.crowdedness_score > max_crowdedness {
            return false;
        }
    }
    if let Some(min_inside_ticks) = config.min_inside_ticks {
        if strategy.inside_ticks < min_inside_ticks {
            return false;
        }
    }

    true
}

fn build_filtered_market(analysis: &MarketAnalysis, strategy: &StrategyAnalysis) -> FilteredMarket {
    let effective_quote_size = analysis.effective_quote_size.max(Decimal::ONE);
    let inside_depth_multiple = strategy.inside_depth / effective_quote_size;
    let max_spread = analysis.rewards_max_spread / Decimal::from(100u32);
    let headroom_ratio = if max_spread > Decimal::ZERO {
        (strategy.reward_gap_headroom / max_spread).clamp(Decimal::ZERO, Decimal::ONE)
    } else {
        Decimal::ZERO
    };

    let inside_ticks_score = (Decimal::from(strategy.inside_ticks) / Decimal::from(4u32))
        .clamp(Decimal::ZERO, Decimal::ONE);
    let inside_depth_score =
        (inside_depth_multiple / Decimal::from(5u32)).clamp(Decimal::ZERO, Decimal::ONE);
    let crowdedness_score =
        Decimal::ONE / (Decimal::ONE + strategy.crowdedness_score.max(Decimal::ZERO));
    let eligible_factor = if analysis.current_config_eligible {
        Decimal::ONE
    } else {
        Decimal::new(5, 1)
    };

    let safety_score = eligible_factor
        * ((Decimal::new(35, 2) * inside_ticks_score)
            + (Decimal::new(30, 2) * inside_depth_score)
            + (Decimal::new(20, 2) * headroom_ratio)
            + (Decimal::new(15, 2) * crowdedness_score));
    let safety_score = safety_score.clamp(Decimal::ZERO, Decimal::ONE);
    let risk_score = Decimal::ONE - safety_score;
    let balanced_score = strategy.roi_daily_conservative * safety_score;

    FilteredMarket {
        market: analysis.clone(),
        strategy: strategy.clone(),
        safety_score,
        risk_score,
        balanced_score,
        inside_depth_multiple,
        headroom_ratio,
    }
}

fn compare_entries(
    left: &FilteredMarket,
    right: &FilteredMarket,
    sort_by: SortKey,
) -> std::cmp::Ordering {
    match sort_by {
        SortKey::RoiDaily => right
            .strategy
            .roi_daily_conservative
            .cmp(&left.strategy.roi_daily_conservative)
            .then_with(|| {
                right
                    .strategy
                    .estimated_daily_reward
                    .cmp(&left.strategy.estimated_daily_reward)
            })
            .then_with(|| {
                left.strategy
                    .crowdedness_score
                    .cmp(&right.strategy.crowdedness_score)
            }),
        SortKey::RoiAnnual => right
            .strategy
            .roi_annual_conservative
            .cmp(&left.strategy.roi_annual_conservative)
            .then_with(|| {
                right
                    .strategy
                    .estimated_daily_reward
                    .cmp(&left.strategy.estimated_daily_reward)
            })
            .then_with(|| {
                left.strategy
                    .crowdedness_score
                    .cmp(&right.strategy.crowdedness_score)
            }),
        SortKey::EstimatedDailyReward => right
            .strategy
            .estimated_daily_reward
            .cmp(&left.strategy.estimated_daily_reward)
            .then_with(|| {
                right
                    .strategy
                    .roi_daily_conservative
                    .cmp(&left.strategy.roi_daily_conservative)
            }),
        SortKey::DailyReward => right
            .market
            .daily_reward
            .cmp(&left.market.daily_reward)
            .then_with(|| {
                right
                    .strategy
                    .roi_daily_conservative
                    .cmp(&left.strategy.roi_daily_conservative)
            }),
        SortKey::Crowdedness => left
            .strategy
            .crowdedness_score
            .cmp(&right.strategy.crowdedness_score)
            .then_with(|| right.safety_score.cmp(&left.safety_score))
            .then_with(|| {
                right
                    .strategy
                    .roi_daily_conservative
                    .cmp(&left.strategy.roi_daily_conservative)
            }),
        SortKey::Safety => right
            .safety_score
            .cmp(&left.safety_score)
            .then_with(|| {
                right
                    .strategy
                    .roi_daily_conservative
                    .cmp(&left.strategy.roi_daily_conservative)
            })
            .then_with(|| {
                left.strategy
                    .crowdedness_score
                    .cmp(&right.strategy.crowdedness_score)
            }),
        SortKey::Balanced => right
            .balanced_score
            .cmp(&left.balanced_score)
            .then_with(|| right.safety_score.cmp(&left.safety_score))
            .then_with(|| {
                right
                    .strategy
                    .roi_daily_conservative
                    .cmp(&left.strategy.roi_daily_conservative)
            }),
    }
}
