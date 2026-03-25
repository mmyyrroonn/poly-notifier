use poly_lp::rewards_analyzer::{MarketAnalysis, Side, StrategyAnalysis, StrategyProfile};
use poly_lp::rewards_filter::{filter_markets, FilterConfig, ProfileSelection, SortKey};
use rust_decimal_macros::dec;

#[test]
fn filter_markets_applies_thresholds_and_eligibility() {
    let analyses = sample_analyses();
    let config = FilterConfig {
        profile: ProfileSelection::OuterLowRisk,
        sort_by: SortKey::Balanced,
        top_n: 10,
        min_roi_daily: Some(dec!(0.05)),
        min_daily_reward: Some(dec!(50)),
        max_crowdedness: Some(dec!(10)),
        min_inside_ticks: Some(2),
        eligible_only: true,
    };

    let filtered = filter_markets(&analyses, &config);

    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].market.slug, "safer-mid-reward");
    assert!(filtered[0].safety_score > dec!(0.5));
    assert!(filtered[0].risk_score < dec!(0.5));
}

#[test]
fn filter_markets_balanced_sort_prefers_safer_high_yield_mix() {
    let analyses = sample_analyses();
    let config = FilterConfig {
        profile: ProfileSelection::OuterLowRisk,
        sort_by: SortKey::Balanced,
        top_n: 10,
        min_roi_daily: None,
        min_daily_reward: None,
        max_crowdedness: None,
        min_inside_ticks: None,
        eligible_only: false,
    };

    let filtered = filter_markets(&analyses, &config);
    let safer_index = filtered
        .iter()
        .position(|entry| entry.market.slug == "safer-mid-reward")
        .expect("safer market present");
    let crowded_index = filtered
        .iter()
        .position(|entry| entry.market.slug == "crowded-high-reward")
        .expect("crowded market present");

    assert_eq!(filtered[0].market.slug, "safer-mid-reward");
    assert!(safer_index < crowded_index);
    assert!(filtered[safer_index].balanced_score > filtered[crowded_index].balanced_score);
}

#[test]
fn filter_markets_supports_daily_reward_sort() {
    let analyses = sample_analyses();
    let config = FilterConfig {
        profile: ProfileSelection::OuterLowRisk,
        sort_by: SortKey::DailyReward,
        top_n: 10,
        min_roi_daily: None,
        min_daily_reward: None,
        max_crowdedness: None,
        min_inside_ticks: None,
        eligible_only: false,
    };

    let filtered = filter_markets(&analyses, &config);

    assert_eq!(filtered[0].market.slug, "not-eligible-big-reward");
    assert_eq!(filtered[1].market.slug, "crowded-high-reward");
}

fn sample_analyses() -> Vec<MarketAnalysis> {
    vec![
        sample_market(
            "crowded-high-reward",
            dec!(100),
            true,
            sample_strategy(dec!(0.10), dec!(10), 1, dec!(200), dec!(50), dec!(0.005)),
        ),
        sample_market(
            "safer-mid-reward",
            dec!(80),
            true,
            sample_strategy(dec!(0.08), dec!(8), 4, dec!(600), dec!(2), dec!(0.02)),
        ),
        sample_market(
            "not-eligible-big-reward",
            dec!(200),
            false,
            sample_strategy(dec!(0.12), dec!(12), 3, dec!(500), dec!(4), dec!(0.015)),
        ),
    ]
}

fn sample_market(
    slug: &str,
    daily_reward: rust_decimal::Decimal,
    current_config_eligible: bool,
    outer_low_risk: StrategyAnalysis,
) -> MarketAnalysis {
    MarketAnalysis {
        condition_id: format!("{slug}-condition"),
        slug: slug.to_string(),
        question: format!("{slug}?"),
        daily_reward,
        gross_annual_reward: daily_reward * dec!(365),
        rewards_min_size: dec!(20),
        rewards_max_spread: dec!(3.5),
        midpoint: dec!(0.50),
        best_bid: Some(dec!(0.49)),
        best_ask: Some(dec!(0.51)),
        current_spread: Some(dec!(0.02)),
        effective_quote_size: dec!(100),
        current_config_eligible,
        existing_q_min: dec!(1000),
        outer_low_risk: Some(outer_low_risk),
        aggressive_mid: None,
    }
}

fn sample_strategy(
    roi_daily: rust_decimal::Decimal,
    est_day: rust_decimal::Decimal,
    inside_ticks: u32,
    inside_depth: rust_decimal::Decimal,
    crowdedness: rust_decimal::Decimal,
    headroom: rust_decimal::Decimal,
) -> StrategyAnalysis {
    StrategyAnalysis {
        profile: StrategyProfile::OuterLowRisk,
        recommended_side: Side::Buy,
        recommended_price: dec!(0.47),
        qualifying_boundary: dec!(0.465),
        inside_ticks,
        inside_depth,
        existing_qualified_ahead_size: crowdedness * dec!(100),
        reward_gap_headroom: headroom,
        estimated_share: dec!(0.1),
        estimated_daily_reward: est_day,
        roi_daily_conservative: roi_daily,
        roi_annual_conservative: roi_daily * dec!(365),
        crowdedness_score: crowdedness,
        our_q_min: dec!(10),
        our_side_score: dec!(30),
    }
}
