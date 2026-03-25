use std::env;

use anyhow::{anyhow, bail, Context, Result};
use pn_common::{
    config::AppConfig,
    db::get_lp_reports_for_condition,
    models::LpReport,
};
use serde::Serialize;
use serde_json::Value;
use sqlx::sqlite::SqlitePoolOptions;

const DEFAULT_CONFIG_PATH: &str = "config/default.toml";

#[derive(Debug, Clone, PartialEq, Eq)]
struct CliArgs {
    condition_id: String,
    report_type: AuditKind,
    limit: i64,
    json: bool,
    config_path: String,
    database_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuditKind {
    All,
    Decision,
    Signal,
    Quote,
    Flatten,
}

impl AuditKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Decision => "decision",
            Self::Signal => "signal",
            Self::Quote => "quote",
            Self::Flatten => "flatten",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct AuditOutputRow {
    report_id: i64,
    report_type: String,
    created_at: String,
    payload: Value,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_cli_args(env::args().skip(1).collect())?;
    let database_url = resolve_database_url(&args)?;
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .with_context(|| format!("connect sqlite database {database_url}"))?;

    let reports = get_lp_reports_for_condition(&pool, &args.condition_id, args.limit)
        .await
        .with_context(|| format!("load lp reports for {}", args.condition_id))?;
    let selected = select_audit_rows(&reports, args.report_type)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&selected)?);
        return Ok(());
    }

    if selected.is_empty() {
        println!(
            "No audit events found for condition_id={} type={}",
            args.condition_id,
            args.report_type.as_str()
        );
        return Ok(());
    }

    println!(
        "Audit timeline for condition_id={} type={} count={}",
        args.condition_id,
        args.report_type.as_str(),
        selected.len()
    );
    for row in selected {
        println!("{}", render_timeline_line(&row.payload)?);
    }

    Ok(())
}

fn parse_cli_args(args: Vec<String>) -> Result<CliArgs> {
    let mut condition_id = None;
    let mut report_type = AuditKind::All;
    let mut limit = 50;
    let mut json = false;
    let mut config_path = DEFAULT_CONFIG_PATH.to_string();
    let mut database_url = None;
    let mut iter = args.into_iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--condition-id" => {
                condition_id = Some(
                    iter.next()
                        .ok_or_else(|| anyhow!("--condition-id requires a value"))?,
                );
            }
            "--type" => {
                let value = iter
                    .next()
                    .ok_or_else(|| anyhow!("--type requires a value"))?;
                report_type = parse_audit_kind(&value)?;
            }
            "--limit" => {
                let value = iter
                    .next()
                    .ok_or_else(|| anyhow!("--limit requires a value"))?;
                limit = value
                    .parse::<i64>()
                    .with_context(|| format!("invalid --limit value {value}"))?;
                if limit <= 0 {
                    bail!("--limit must be greater than 0");
                }
            }
            "--json" => json = true,
            "--config" => {
                config_path = iter
                    .next()
                    .ok_or_else(|| anyhow!("--config requires a path"))?;
            }
            "--db" => {
                database_url = Some(
                    iter.next()
                        .ok_or_else(|| anyhow!("--db requires a sqlite url"))?,
                );
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            _ => bail!("unexpected argument '{arg}'"),
        }
    }

    Ok(CliArgs {
        condition_id: condition_id.ok_or_else(|| anyhow!("missing --condition-id"))?,
        report_type,
        limit,
        json,
        config_path,
        database_url,
    })
}

fn parse_audit_kind(value: &str) -> Result<AuditKind> {
    match value.trim().to_ascii_lowercase().as_str() {
        "all" => Ok(AuditKind::All),
        "decision" => Ok(AuditKind::Decision),
        "signal" => Ok(AuditKind::Signal),
        "quote" => Ok(AuditKind::Quote),
        "flatten" => Ok(AuditKind::Flatten),
        other => bail!("invalid --type '{other}', expected all|decision|signal|quote|flatten"),
    }
}

fn print_usage() {
    println!(
        "Usage: poly-lp-audit --condition-id <id> [--type all|decision|signal|quote|flatten] [--limit N] [--json] [--config path] [--db sqlite:url]"
    );
}

fn resolve_database_url(args: &CliArgs) -> Result<String> {
    if let Some(url) = &args.database_url {
        return Ok(url.clone());
    }

    let config = AppConfig::load_from(&args.config_path)
        .with_context(|| format!("load config from {}", args.config_path))?;
    Ok(config.database.url)
}

fn select_audit_rows(reports: &[LpReport], kind: AuditKind) -> Result<Vec<AuditOutputRow>> {
    let mut selected = Vec::new();

    for report in reports.iter().rev() {
        if !matches_audit_kind(kind, &report.report_type) {
            continue;
        }
        let payload: Value = serde_json::from_str(&report.payload).with_context(|| {
            format!(
                "parse audit payload for report {} ({})",
                report.id, report.report_type
            )
        })?;
        if payload
            .get("audit")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            selected.push(AuditOutputRow {
                report_id: report.id,
                report_type: report.report_type.clone(),
                created_at: report.created_at.to_string(),
                payload,
            });
        }
    }

    Ok(selected)
}

fn matches_audit_kind(kind: AuditKind, report_type: &str) -> bool {
    match kind {
        AuditKind::All => report_type.ends_with("_audit"),
        AuditKind::Decision => report_type == "decision_audit",
        AuditKind::Signal => report_type == "signal_audit",
        AuditKind::Quote => report_type == "quote_audit",
        AuditKind::Flatten => report_type == "flatten_audit",
    }
}

fn render_timeline_line(payload: &Value) -> Result<String> {
    let recorded_at = required_str(payload, "recorded_at")?;
    let event_type = required_str(payload, "event_type")?;
    let reason = payload
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("n/a");

    let positions = payload
        .pointer("/state/positions")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    format!(
                        "{}:{}@{}",
                        row.get("asset_id")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown"),
                        row.get("size").and_then(Value::as_str).unwrap_or("?"),
                        row.get("avg_price").and_then(Value::as_str).unwrap_or("?"),
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "flat".to_string());

    let books = payload
        .pointer("/state/books")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    let bid = row
                        .pointer("/best_bid/price")
                        .and_then(Value::as_str)
                        .unwrap_or("-");
                    let ask = row
                        .pointer("/best_ask/price")
                        .and_then(Value::as_str)
                        .unwrap_or("-");
                    format!(
                        "{} {}/{}",
                        row.get("asset_id")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown"),
                        bid,
                        ask
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "no-books".to_string());

    let action = match event_type {
        "decision_updated" => summarize_decision(payload),
        "signal_transition" => summarize_signal(payload),
        "quote_submitted" | "quote_cancelled" => summarize_quotes(payload),
        "flatten_submitted" | "flatten_error" => summarize_flatten(payload),
        _ => String::new(),
    };

    Ok(format!(
        "{recorded_at} | {event_type} | reason={reason} | books={books} | positions={positions}{action}",
    ))
}

fn summarize_decision(payload: &Value) -> String {
    let desired_quotes = payload
        .pointer("/decision/desired_quotes")
        .and_then(Value::as_array)
        .map(|quotes| {
            quotes
                .iter()
                .map(format_quote)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "none".to_string());
    format!(" | desired_quotes={desired_quotes}")
}

fn summarize_signal(payload: &Value) -> String {
    let signal = payload.pointer("/signal/name").and_then(Value::as_str);
    let active = payload.pointer("/signal/active").and_then(Value::as_bool);
    match (signal, active) {
        (Some(signal), Some(active)) => format!(" | signal={signal} active={active}"),
        _ => String::new(),
    }
}

fn summarize_quotes(payload: &Value) -> String {
    let action = payload
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let quotes = payload
        .get("quotes")
        .and_then(Value::as_array)
        .map(|quotes| quotes.iter().map(format_quote).collect::<Vec<_>>().join(", "))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "none".to_string());
    format!(" | action={action} quotes={quotes}")
}

fn summarize_flatten(payload: &Value) -> String {
    let flatten = payload.get("flatten").unwrap_or(&Value::Null);
    let asset_id = flatten
        .get("asset_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let side = flatten.get("side").and_then(Value::as_str).unwrap_or("?");
    let size = flatten.get("size").and_then(Value::as_str).unwrap_or("?");
    let error = payload
        .get("error")
        .and_then(Value::as_str)
        .map(|value| format!(" error={value}"))
        .unwrap_or_default();
    format!(" | flatten={asset_id} {side} {size}{error}")
}

fn format_quote(value: &Value) -> String {
    let asset_id = value
        .get("asset_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let side = value.get("side").and_then(Value::as_str).unwrap_or("?");
    let size = value.get("size").and_then(Value::as_str).unwrap_or("?");
    let price = value.get("price").and_then(Value::as_str).unwrap_or("?");
    format!("{asset_id} {side} {size} @ {price}")
}

fn required_str<'a>(payload: &'a Value, key: &str) -> Result<&'a str> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("audit payload missing string field {key}"))
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDateTime;
    use pn_common::models::LpReport;
    use serde_json::{json, Value};

    use super::{
        matches_audit_kind, parse_cli_args, render_timeline_line, select_audit_rows, AuditKind,
        CliArgs,
    };

    #[test]
    fn parse_cli_args_supports_condition_type_limit_and_json() {
        let parsed = parse_cli_args(vec![
            "--condition-id".to_string(),
            "condition-1".to_string(),
            "--type".to_string(),
            "flatten".to_string(),
            "--limit".to_string(),
            "25".to_string(),
            "--json".to_string(),
            "--config".to_string(),
            "config/custom.toml".to_string(),
            "--db".to_string(),
            "sqlite:/tmp/audit.db".to_string(),
        ])
        .expect("args parsed");

        assert_eq!(
            parsed,
            CliArgs {
                condition_id: "condition-1".to_string(),
                report_type: AuditKind::Flatten,
                limit: 25,
                json: true,
                config_path: "config/custom.toml".to_string(),
                database_url: Some("sqlite:/tmp/audit.db".to_string()),
            }
        );
    }

    #[test]
    fn matches_audit_kind_filters_expected_report_types() {
        assert!(matches_audit_kind(AuditKind::All, "decision_audit"));
        assert!(matches_audit_kind(AuditKind::Decision, "decision_audit"));
        assert!(matches_audit_kind(AuditKind::Signal, "signal_audit"));
        assert!(!matches_audit_kind(AuditKind::Signal, "quote_audit"));
        assert!(matches_audit_kind(AuditKind::Flatten, "flatten_audit"));
        assert!(matches_audit_kind(AuditKind::Quote, "quote_audit"));
        assert!(!matches_audit_kind(AuditKind::All, "summary"));
    }

    #[test]
    fn render_timeline_line_surfaces_reason_and_market_state() {
        let rendered = render_timeline_line(&json!({
            "recorded_at": "2026-03-25T10:00:00Z",
            "event_type": "decision_updated",
            "reason": "reward-qualified single-sided quote",
            "state": {
                "positions": [{ "asset_id": "asset-yes", "size": "25", "avg_price": "0.42" }],
                "books": [{ "asset_id": "asset-yes", "best_bid": { "price": "0.50", "size": "100" }, "best_ask": { "price": "0.51", "size": "120" } }]
            },
            "decision": {
                "desired_quotes": [{ "asset_id": "asset-yes", "side": "Sell", "price": "0.52", "size": "10", "reason": "reward-aware quote" }]
            }
        }))
        .expect("timeline rendered");

        assert!(rendered.contains("2026-03-25T10:00:00Z"));
        assert!(rendered.contains("decision_updated"));
        assert!(rendered.contains("reward-qualified single-sided quote"));
        assert!(rendered.contains("asset-yes"));
        assert!(rendered.contains("0.50/0.51"));
        assert!(rendered.contains("Sell 10 @ 0.52"));
    }

    #[test]
    fn select_audit_rows_keeps_only_matching_audit_entries() {
        let reports = vec![
            report(
                1,
                "decision_audit",
                json!({ "audit": true, "event_type": "decision_updated", "recorded_at": "2026-03-25T10:00:00Z" }),
            ),
            report(2, "summary", json!({ "audit": false })),
            report(
                3,
                "signal_audit",
                json!({ "audit": true, "event_type": "signal_transition", "recorded_at": "2026-03-25T10:01:00Z" }),
            ),
        ];

        let rows = select_audit_rows(&reports, AuditKind::Signal).expect("rows selected");

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].report_type, "signal_audit");
        assert_eq!(rows[0].payload["event_type"], "signal_transition");
    }

    fn report(id: i64, report_type: &str, payload: Value) -> LpReport {
        LpReport {
            id,
            condition_id: Some("condition-1".to_string()),
            report_type: report_type.to_string(),
            payload: payload.to_string(),
            created_at: NaiveDateTime::parse_from_str("2026-03-25 10:00:00", "%Y-%m-%d %H:%M:%S")
                .expect("naive timestamp"),
        }
    }
}
