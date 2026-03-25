use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

use pn_common::config::AppConfig;
use poly_lp::rewards_analyzer::{MarketAnalysis, Side, StrategyAnalysis, StrategyProfile};
use poly_lp::rewards_filter::{render_multi_market_config, FilteredMarket};
use rust_decimal_macros::dec;

#[test]
fn render_multi_market_config_emits_runnable_market_blocks_and_reference_comments() {
    let rendered = render_multi_market_config(
        include_str!("../../../config/default.toml"),
        "outputs/rewards/report.json",
        "outer_low_risk",
        "balanced",
        &[filtered_market()],
    )
    .expect("render config");

    assert!(rendered.contains("[[lp.markets]]"));
    assert!(rendered.contains("condition_id = \"condition-1\""));
    assert!(rendered.contains("quote_size = 20.0"));
    assert!(rendered.contains("# rewards_min_size = 20"));
    assert!(rendered.contains("# recommended_price = 0.466"));
    assert!(rendered.contains("# approx_one_quote_buy_cost_usdc = 10.60"));
    assert!(rendered.contains("question = \"Will X happen?\\nLine two\\tTabbed\""));

    let temp_path = std::env::temp_dir().join(format!(
        "poly-rewards-runtime-config-{}-{}.toml",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after unix epoch")
            .as_nanos()
    ));
    fs::write(&temp_path, &rendered).expect("write temp config");
    let parsed = AppConfig::load_from(temp_path.to_str().expect("temp path should be valid utf-8"))
        .expect("generated config should parse");
    fs::remove_file(&temp_path).expect("remove temp config");

    assert_eq!(parsed.lp.markets.len(), 1);
    assert_eq!(parsed.lp.markets[0].condition_id, "condition-1");
    assert_eq!(
        parsed.lp.markets[0]
            .strategy
            .as_ref()
            .and_then(|strategy| strategy.quote_size),
        Some(20.0)
    );
}

fn filtered_market() -> FilteredMarket {
    FilteredMarket {
        market: MarketAnalysis {
            condition_id: "condition-1".to_string(),
            slug: "market-1".to_string(),
            question: "Will X happen?\nLine two\tTabbed".to_string(),
            daily_reward: dec!(50),
            gross_annual_reward: dec!(18250),
            rewards_min_size: dec!(20),
            rewards_max_spread: dec!(3.5),
            midpoint: dec!(0.53),
            best_bid: Some(dec!(0.52)),
            best_ask: Some(dec!(0.54)),
            current_spread: Some(dec!(0.02)),
            effective_quote_size: dec!(20),
            current_config_eligible: true,
            existing_q_min: dec!(10),
            outer_low_risk: Some(strategy()),
            aggressive_mid: None,
        },
        strategy: strategy(),
        safety_score: dec!(0.8),
        risk_score: dec!(0.2),
        balanced_score: dec!(0.04),
        inside_depth_multiple: dec!(2.5),
        headroom_ratio: dec!(0.4),
    }
}

fn strategy() -> StrategyAnalysis {
    StrategyAnalysis {
        profile: StrategyProfile::OuterLowRisk,
        recommended_side: Side::Buy,
        recommended_price: dec!(0.466),
        qualifying_boundary: dec!(0.47),
        inside_ticks: 2,
        inside_depth: dec!(50),
        existing_qualified_ahead_size: dec!(5),
        reward_gap_headroom: dec!(0.01),
        estimated_share: dec!(0.15),
        estimated_daily_reward: dec!(1.2),
        roi_daily_conservative: dec!(0.06),
        roi_annual_conservative: dec!(21.9),
        crowdedness_score: dec!(0.25),
        our_q_min: dec!(2),
        our_side_score: dec!(3),
    }
}
