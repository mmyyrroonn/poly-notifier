use std::{env, fs, path::PathBuf, str::FromStr};

use anyhow::{anyhow, bail, Context, Result};
use poly_lp::rewards_filter::{
    filter_markets, FilterConfig, FilteredMarket, ProfileSelection, SortKey,
};
use poly_lp::rewards_report::RewardsReport;
use rust_decimal::Decimal;
use serde::Serialize;

const DEFAULT_REPORT_PATH: &str = "outputs/rewards/report.json";

#[derive(Debug, Clone)]
struct CliArgs {
    report_path: PathBuf,
    profile: ProfileSelection,
    sort_by: SortKey,
    top_n: usize,
    min_roi_daily: Option<Decimal>,
    min_daily_reward: Option<Decimal>,
    max_crowdedness: Option<Decimal>,
    min_inside_ticks: Option<u32>,
    eligible_only: bool,
    out_path: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct FilterOutput {
    source_report: String,
    profile: ProfileSelection,
    sort_by: SortKey,
    count: usize,
    entries: Vec<FilteredMarket>,
}

fn main() -> Result<()> {
    let args = parse_cli_args(env::args().skip(1).collect())?;
    let report = load_report(&args.report_path)?;
    let config = FilterConfig {
        profile: args.profile,
        sort_by: args.sort_by,
        top_n: args.top_n,
        min_roi_daily: args.min_roi_daily,
        min_daily_reward: args.min_daily_reward,
        max_crowdedness: args.max_crowdedness,
        min_inside_ticks: args.min_inside_ticks,
        eligible_only: args.eligible_only,
    };
    let filtered = filter_markets(&report.analyses, &config);

    print_summary(&args, &report, &filtered);

    if let Some(out_path) = &args.out_path {
        let output = FilterOutput {
            source_report: args.report_path.display().to_string(),
            profile: args.profile,
            sort_by: args.sort_by,
            count: filtered.len(),
            entries: filtered,
        };
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::write(out_path, serde_json::to_vec_pretty(&output)?)
            .with_context(|| format!("failed to write {}", out_path.display()))?;
        println!("Filtered output: {}", out_path.display());
    }

    Ok(())
}

fn parse_cli_args(args: Vec<String>) -> Result<CliArgs> {
    let mut report_path = PathBuf::from(DEFAULT_REPORT_PATH);
    let mut profile = ProfileSelection::OuterLowRisk;
    let mut sort_by = SortKey::Balanced;
    let mut top_n = 25usize;
    let mut min_roi_daily = None;
    let mut min_daily_reward = None;
    let mut max_crowdedness = None;
    let mut min_inside_ticks = None;
    let mut eligible_only = false;
    let mut out_path = None;

    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--report" => {
                report_path = PathBuf::from(
                    iter.next()
                        .ok_or_else(|| anyhow!("--report requires a path"))?,
                );
            }
            "--profile" => {
                profile = parse_profile_selection(
                    &iter
                        .next()
                        .ok_or_else(|| anyhow!("--profile requires a value"))?,
                )?;
            }
            "--sort" => {
                sort_by = parse_sort_key(
                    &iter
                        .next()
                        .ok_or_else(|| anyhow!("--sort requires a value"))?,
                )?;
            }
            "--top-n" => {
                top_n = iter
                    .next()
                    .ok_or_else(|| anyhow!("--top-n requires a value"))?
                    .parse::<usize>()
                    .context("top-n must be a positive integer")?;
            }
            "--min-roi-daily" => {
                min_roi_daily = Some(parse_decimal_arg(
                    "--min-roi-daily",
                    &iter
                        .next()
                        .ok_or_else(|| anyhow!("--min-roi-daily requires a value"))?,
                )?);
            }
            "--min-daily-reward" => {
                min_daily_reward = Some(parse_decimal_arg(
                    "--min-daily-reward",
                    &iter
                        .next()
                        .ok_or_else(|| anyhow!("--min-daily-reward requires a value"))?,
                )?);
            }
            "--max-crowdedness" => {
                max_crowdedness = Some(parse_decimal_arg(
                    "--max-crowdedness",
                    &iter
                        .next()
                        .ok_or_else(|| anyhow!("--max-crowdedness requires a value"))?,
                )?);
            }
            "--min-inside-ticks" => {
                min_inside_ticks = Some(
                    iter.next()
                        .ok_or_else(|| anyhow!("--min-inside-ticks requires a value"))?
                        .parse::<u32>()
                        .context("min-inside-ticks must be a non-negative integer")?,
                );
            }
            "--eligible-only" => eligible_only = true,
            "--out" => {
                out_path = Some(PathBuf::from(
                    iter.next()
                        .ok_or_else(|| anyhow!("--out requires a path"))?,
                ));
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

    Ok(CliArgs {
        report_path,
        profile,
        sort_by,
        top_n,
        min_roi_daily,
        min_daily_reward,
        max_crowdedness,
        min_inside_ticks,
        eligible_only,
        out_path,
    })
}

fn parse_profile_selection(value: &str) -> Result<ProfileSelection> {
    match value {
        "outer_low_risk" => Ok(ProfileSelection::OuterLowRisk),
        "aggressive_mid" => Ok(ProfileSelection::AggressiveMid),
        _ => bail!("invalid profile '{value}', expected outer_low_risk or aggressive_mid"),
    }
}

fn parse_sort_key(value: &str) -> Result<SortKey> {
    match value {
        "balanced" => Ok(SortKey::Balanced),
        "roi_daily" => Ok(SortKey::RoiDaily),
        "roi_annual" => Ok(SortKey::RoiAnnual),
        "estimated_daily_reward" => Ok(SortKey::EstimatedDailyReward),
        "daily_reward" => Ok(SortKey::DailyReward),
        "crowdedness" => Ok(SortKey::Crowdedness),
        "safety" => Ok(SortKey::Safety),
        _ => bail!(
            "invalid sort key '{value}', expected balanced, roi_daily, roi_annual, estimated_daily_reward, daily_reward, crowdedness, or safety"
        ),
    }
}

fn parse_decimal_arg(flag: &str, value: &str) -> Result<Decimal> {
    Decimal::from_str(value).with_context(|| format!("{flag} must be a decimal number"))
}

fn load_report(path: &PathBuf) -> Result<RewardsReport> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("failed to parse {}", path.display()))
}

fn print_usage() {
    println!(
        "Usage: poly-rewards-filter [--report path] [--profile outer_low_risk|aggressive_mid] [--sort balanced|roi_daily|roi_annual|estimated_daily_reward|daily_reward|crowdedness|safety] [--top-n N] [--min-roi-daily X] [--min-daily-reward X] [--max-crowdedness X] [--min-inside-ticks N] [--eligible-only] [--out path]"
    );
    println!("Filters an existing rewards report without fetching live market data.");
}

fn print_summary(args: &CliArgs, report: &RewardsReport, filtered: &[FilteredMarket]) {
    println!(
        "Loaded {} analyzed markets from {}",
        report.analyzed_count,
        args.report_path.display()
    );
    println!(
        "Profile {:?} | sort {:?} | selected {}",
        args.profile,
        args.sort_by,
        filtered.len()
    );

    for (index, entry) in filtered.iter().enumerate() {
        println!(
            "  {}. {} | roi/day {} | est/day {} | safety {} | risk {} | crowd {} | ticks {} | reward/day {}",
            index + 1,
            entry.market.slug,
            entry.strategy.roi_daily_conservative,
            entry.strategy.estimated_daily_reward,
            entry.safety_score,
            entry.risk_score,
            entry.strategy.crowdedness_score,
            entry.strategy.inside_ticks,
            entry.market.daily_reward
        );
    }
}
