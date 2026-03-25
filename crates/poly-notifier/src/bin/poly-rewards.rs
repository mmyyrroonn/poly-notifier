use std::{env, fs, path::PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use futures_util::stream::{self, StreamExt};
use pn_common::config::AppConfig;
use pn_polymarket::{ClobClient, GammaClient, GammaMarketSummary};
use poly_lp::rewards_analyzer::{
    active_daily_reward, analyze_market, render_shortlist_toml, reward_distribution,
    AnalyzerConfig, MarketAnalysis, RewardDistribution, StrategyAnalysis,
};
use rust_decimal::Decimal;
use serde::Serialize;

const DEFAULT_CONFIG_PATH: &str = "config/default.toml";
const DEFAULT_OUT_DIR: &str = "outputs/rewards";
const PAGE_LIMIT: usize = 500;
const BOOK_FETCH_CONCURRENCY: usize = 16;

#[derive(Debug, Clone)]
struct CliArgs {
    config_path: String,
    out_dir: PathBuf,
    top_n: usize,
    max_markets: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
struct SkippedMarket {
    condition_id: String,
    slug: String,
    question: String,
    reason: String,
}

#[derive(Debug, Serialize)]
struct RewardsReport {
    generated_at: String,
    scan_count: usize,
    analyzed_count: usize,
    skipped_count: usize,
    distribution: Option<RewardDistribution>,
    analyses: Vec<MarketAnalysis>,
    skipped: Vec<SkippedMarket>,
}

enum AnalysisOutcome {
    Analysis(MarketAnalysis),
    Skipped(SkippedMarket),
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_cli_args(env::args().skip(1).collect())?;
    let app_config = AppConfig::load_from(&args.config_path)
        .with_context(|| format!("failed to load config from {}", args.config_path))?;
    let analyzer_config = analyzer_config_from_app(&app_config, args.top_n)?;

    let gamma = GammaClient::with_base_url(app_config.lp.trading.gamma_base_url.clone());
    let clob = ClobClient::with_base_url(app_config.lp.trading.clob_base_url.clone());

    let reward_markets = fetch_reward_markets(&gamma, args.max_markets).await?;
    let generated_at = Utc::now().to_rfc3339();

    let outcomes = stream::iter(reward_markets.into_iter().map(|market| {
        let clob = clob.clone();
        let analyzer_config = analyzer_config.clone();
        async move { analyze_reward_market(market, clob, analyzer_config).await }
    }))
    .buffer_unordered(BOOK_FETCH_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    let mut analyses = Vec::new();
    let mut skipped = Vec::new();
    for outcome in outcomes {
        match outcome {
            AnalysisOutcome::Analysis(analysis) => analyses.push(analysis),
            AnalysisOutcome::Skipped(skip) => skipped.push(skip),
        }
    }

    analyses.sort_by(|left, right| right.daily_reward.cmp(&left.daily_reward));
    skipped.sort_by(|left, right| left.condition_id.cmp(&right.condition_id));
    let distribution = reward_distribution(&analyses);

    fs::create_dir_all(&args.out_dir)
        .with_context(|| format!("failed to create {}", args.out_dir.display()))?;
    let report_path = args.out_dir.join("report.json");
    let shortlist_path = args.out_dir.join("shortlist.toml");

    let report = RewardsReport {
        generated_at: generated_at.clone(),
        scan_count: analyses.len() + skipped.len(),
        analyzed_count: analyses.len(),
        skipped_count: skipped.len(),
        distribution,
        analyses: analyses.clone(),
        skipped,
    };
    fs::write(&report_path, serde_json::to_vec_pretty(&report)?)
        .with_context(|| format!("failed to write {}", report_path.display()))?;

    let shortlist = render_shortlist_toml(&generated_at, &analyzer_config, &analyses);
    fs::write(&shortlist_path, shortlist.as_bytes())
        .with_context(|| format!("failed to write {}", shortlist_path.display()))?;

    print_summary(&report, &analyzer_config, &report_path, &shortlist_path);
    Ok(())
}

fn parse_cli_args(args: Vec<String>) -> Result<CliArgs> {
    let mut config_path = DEFAULT_CONFIG_PATH.to_string();
    let mut out_dir = PathBuf::from(DEFAULT_OUT_DIR);
    let mut top_n = 25usize;
    let mut max_markets = None;
    let mut iter = args.into_iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config" => {
                config_path = iter
                    .next()
                    .ok_or_else(|| anyhow!("--config requires a path"))?;
            }
            "--out-dir" => {
                out_dir = PathBuf::from(
                    iter.next()
                        .ok_or_else(|| anyhow!("--out-dir requires a path"))?,
                );
            }
            "--top-n" => {
                top_n = iter
                    .next()
                    .ok_or_else(|| anyhow!("--top-n requires a value"))?
                    .parse::<usize>()
                    .context("top-n must be a positive integer")?;
            }
            "--max-markets" => {
                max_markets = Some(
                    iter.next()
                        .ok_or_else(|| anyhow!("--max-markets requires a value"))?
                        .parse::<usize>()
                        .context("max-markets must be a positive integer")?,
                );
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => bail!("unexpected argument '{other}'"),
        }
    }

    if top_n == 0 {
        bail!("--top-n must be greater than 0");
    }
    if max_markets == Some(0) {
        bail!("--max-markets must be greater than 0");
    }

    Ok(CliArgs {
        config_path,
        out_dir,
        top_n,
        max_markets,
    })
}

fn print_usage() {
    println!(
        "Usage: poly-rewards [--config path] [--out-dir path] [--top-n N] [--max-markets N]"
    );
    println!("Scans all active reward markets, analyzes order-book crowding, and writes report files.");
}

fn analyzer_config_from_app(app_config: &AppConfig, top_n: usize) -> Result<AnalyzerConfig> {
    Ok(AnalyzerConfig {
        quote_size: decimal_from_f64(app_config.lp.strategy.quote_size)?,
        quote_offset_ticks: app_config.lp.strategy.quote_offset_ticks,
        min_inside_ticks: app_config.lp.strategy.min_inside_ticks,
        min_inside_depth_multiple: decimal_from_f64(
            app_config.lp.strategy.min_inside_depth_multiple,
        )?,
        top_n,
    })
}

fn decimal_from_f64(value: f64) -> Result<Decimal> {
    value
        .to_string()
        .parse::<Decimal>()
        .with_context(|| format!("invalid decimal {value}"))
}

async fn fetch_reward_markets(
    client: &GammaClient,
    max_markets: Option<usize>,
) -> Result<Vec<GammaMarketSummary>> {
    let mut markets = Vec::new();
    let mut offset = 0usize;

    loop {
        let page = client
            .list_markets_page(true, false, PAGE_LIMIT, offset)
            .await?;
        if page.is_empty() {
            break;
        }

        let page_len = page.len();
        markets.extend(page.into_iter().filter(|market| {
            active_daily_reward(market) > Decimal::ZERO
                && market.rewards_min_size.is_some()
                && market.rewards_max_spread.is_some()
        }));
        if let Some(limit) = max_markets {
            if markets.len() >= limit {
                markets.truncate(limit);
                break;
            }
        }

        if page_len < PAGE_LIMIT {
            break;
        }
        offset += page_len;
    }

    Ok(markets)
}

async fn analyze_reward_market(
    market: GammaMarketSummary,
    clob: ClobClient,
    analyzer_config: AnalyzerConfig,
) -> AnalysisOutcome {
    let yes_token = match yes_token_id(&market) {
        Some(token) => token,
        None => {
            return AnalysisOutcome::Skipped(skip_market(
                &market,
                "unsupported_outcome_layout".to_string(),
            ))
        }
    };

    let book = match clob.get_order_book(&yes_token).await {
        Ok(book) => book,
        Err(error) => {
            return AnalysisOutcome::Skipped(skip_market(
                &market,
                format!("book_fetch_failed: {error:#}"),
            ))
        }
    };

    match analyze_market(&market, &book, &analyzer_config) {
        Ok(analysis) => AnalysisOutcome::Analysis(analysis),
        Err(error) => AnalysisOutcome::Skipped(skip_market(
            &market,
            format!("analysis_failed: {error:#}"),
        )),
    }
}

fn yes_token_id(market: &GammaMarketSummary) -> Option<String> {
    if market.tokens.len() != 2 {
        return None;
    }

    let has_yes = market
        .tokens
        .iter()
        .any(|token| token.outcome.eq_ignore_ascii_case("yes"));
    let has_no = market
        .tokens
        .iter()
        .any(|token| token.outcome.eq_ignore_ascii_case("no"));
    if !has_yes || !has_no {
        return None;
    }

    market
        .tokens
        .iter()
        .find(|token| token.outcome.eq_ignore_ascii_case("yes"))
        .map(|token| token.token_id.clone())
}

fn skip_market(market: &GammaMarketSummary, reason: String) -> SkippedMarket {
    SkippedMarket {
        condition_id: market.condition_id.clone(),
        slug: market.slug.clone(),
        question: market.question.clone(),
        reason,
    }
}

fn print_summary(
    report: &RewardsReport,
    analyzer_config: &AnalyzerConfig,
    report_path: &PathBuf,
    shortlist_path: &PathBuf,
) {
    println!(
        "Scanned {} reward markets: analyzed {}, skipped {}",
        report.scan_count, report.analyzed_count, report.skipped_count
    );
    if let Some(distribution) = &report.distribution {
        println!(
            "Daily reward distribution: min {} median {} p90 {} max {}",
            distribution.min, distribution.median, distribution.p90, distribution.max
        );
    }

    print_top_daily_reward_markets(&report.analyses, analyzer_config.top_n.min(5));
    print_top_profile(
        "outer_low_risk",
        report
            .analyses
            .iter()
            .filter_map(|analysis| analysis.outer_low_risk.as_ref().map(|profile| (analysis, profile)))
            .collect(),
        analyzer_config.top_n.min(5),
    );
    print_top_profile(
        "aggressive_mid",
        report
            .analyses
            .iter()
            .filter_map(|analysis| analysis.aggressive_mid.as_ref().map(|profile| (analysis, profile)))
            .collect(),
        analyzer_config.top_n.min(5),
    );

    println!("Report: {}", report_path.display());
    println!("Shortlist: {}", shortlist_path.display());
}

fn print_top_daily_reward_markets(analyses: &[MarketAnalysis], count: usize) {
    println!("Top daily reward markets:");
    for (index, analysis) in analyses.iter().take(count).enumerate() {
        println!(
            "  {}. {} | reward/day {} | min_size {} | midpoint {}",
            index + 1,
            analysis.slug,
            analysis.daily_reward,
            analysis.rewards_min_size,
            analysis.midpoint
        );
    }
}

fn print_top_profile(
    name: &str,
    mut entries: Vec<(&MarketAnalysis, &StrategyAnalysis)>,
    count: usize,
) {
    entries.sort_by(|left, right| {
        right
            .1
            .roi_daily_conservative
            .cmp(&left.1.roi_daily_conservative)
            .then_with(|| right.1.estimated_daily_reward.cmp(&left.1.estimated_daily_reward))
    });

    println!("Top {} candidates:", name);
    for (index, (analysis, profile)) in entries.into_iter().take(count).enumerate() {
        println!(
            "  {}. {} | side {:?} @ {} | roi/day {} | est/day {} | inside_ticks {} | crowd {}",
            index + 1,
            analysis.slug,
            profile.recommended_side,
            profile.recommended_price,
            profile.roi_daily_conservative,
            profile.estimated_daily_reward,
            profile.inside_ticks,
            profile.crowdedness_score
        );
    }
}
