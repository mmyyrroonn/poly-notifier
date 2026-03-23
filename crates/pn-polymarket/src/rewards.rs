use std::future::Future;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::NaiveDate;
use reqwest::Client;
use rust_decimal::Decimal;
use serde::Deserialize;

const DEFAULT_BASE_URL: &str = "https://clob.polymarket.com";

#[derive(Debug, Clone)]
pub struct RewardClient {
    http: Client,
    base_url: String,
}

impl RewardClient {
    pub fn new() -> Self {
        Self {
            http: Client::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            http: Client::new(),
            base_url: base_url.into(),
        }
    }

    pub async fn fetch_market_snapshot(
        &self,
        condition_id: &str,
        sponsored: bool,
        attempts: usize,
        backoff: Duration,
        today: NaiveDate,
    ) -> Result<Option<RewardSnapshot>> {
        let response = retry_with_backoff(attempts, backoff, || async {
            self.fetch_market_response(condition_id, sponsored).await
        })
        .await?;
        Ok(build_reward_snapshot(&response, today))
    }

    async fn fetch_market_response(
        &self,
        condition_id: &str,
        sponsored: bool,
    ) -> Result<RewardMarketResponse> {
        let url = format!("{}/rewards/markets/{condition_id}", self.base_url);
        let response = self
            .http
            .get(&url)
            .query(&[("sponsored", sponsored)])
            .send()
            .await
            .with_context(|| format!("GET {url} failed"))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .with_context(|| "reading reward market response body")?;

        if !status.is_success() {
            anyhow::bail!("reward API returned {status} for condition_id '{condition_id}': {body}");
        }

        parse_reward_market_response(&body)
    }
}

impl Default for RewardClient {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RewardMarketResponse {
    #[serde(default)]
    pub data: Vec<RewardMarketEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RewardMarketEntry {
    pub condition_id: String,
    pub question: String,
    pub market_slug: String,
    pub event_slug: String,
    pub rewards_max_spread: i64,
    pub rewards_min_size: i64,
    #[serde(default)]
    pub tokens: Vec<RewardTokenPrice>,
    #[serde(default)]
    pub rewards_config: Vec<RewardConfigEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RewardTokenPrice {
    pub token_id: String,
    pub outcome: String,
    pub price: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RewardConfigEntry {
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    pub rate_per_day: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RewardTokenSnapshot {
    pub token_id: String,
    pub outcome: String,
    pub price: Decimal,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RewardSnapshot {
    pub condition_id: String,
    pub question: String,
    pub market_slug: String,
    pub event_slug: String,
    pub max_spread: Decimal,
    pub min_size: Decimal,
    pub total_daily_rate: Decimal,
    pub token_prices: Vec<RewardTokenSnapshot>,
    pub active_until: NaiveDate,
}

pub(crate) fn build_reward_snapshot(
    response: &RewardMarketResponse,
    today: NaiveDate,
) -> Option<RewardSnapshot> {
    let market = response.data.first()?;
    let active_configs = market
        .rewards_config
        .iter()
        .filter(|config| config.start_date <= today && config.end_date >= today)
        .collect::<Vec<_>>();

    if active_configs.is_empty() {
        return None;
    }

    let total_daily_rate = active_configs.iter().fold(Decimal::ZERO, |sum, config| {
        sum + decimal_from_float(config.rate_per_day)
    });
    if total_daily_rate <= Decimal::ZERO {
        return None;
    }

    let active_until = active_configs
        .iter()
        .map(|config| config.end_date)
        .max()
        .expect("active config list is non-empty");

    Some(RewardSnapshot {
        condition_id: market.condition_id.clone(),
        question: market.question.clone(),
        market_slug: market.market_slug.clone(),
        event_slug: market.event_slug.clone(),
        max_spread: Decimal::new(market.rewards_max_spread, 2),
        min_size: Decimal::from(market.rewards_min_size),
        total_daily_rate,
        token_prices: market
            .tokens
            .iter()
            .map(|token| RewardTokenSnapshot {
                token_id: token.token_id.clone(),
                outcome: token.outcome.clone(),
                price: decimal_from_float(token.price),
            })
            .collect(),
        active_until,
    })
}

pub(crate) async fn retry_with_backoff<T, F, Fut>(
    attempts: usize,
    base_delay: Duration,
    mut operation: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let attempts = attempts.max(1);
    let mut last_error = None;

    for attempt in 1..=attempts {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(error) => {
                last_error = Some(error);
                if attempt < attempts {
                    tokio::time::sleep(scale_delay(base_delay, attempt as u32)).await;
                }
            }
        }
    }

    Err(last_error.expect("retry loop must record an error"))
}

fn scale_delay(base_delay: Duration, multiplier: u32) -> Duration {
    base_delay.checked_mul(multiplier).unwrap_or(Duration::MAX)
}

fn decimal_from_float(value: f64) -> Decimal {
    Decimal::from_str_exact(&value.to_string())
        .with_context(|| format!("failed to convert '{value}' to Decimal"))
        .expect("reward API decimals should parse")
}

fn parse_reward_market_response(body: &str) -> Result<RewardMarketResponse> {
    serde_json::from_str(body).with_context(|| "parsing reward market response JSON")
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;

    use super::{
        build_reward_snapshot, parse_reward_market_response, retry_with_backoff, RewardConfigEntry,
        RewardMarketEntry, RewardMarketResponse, RewardTokenPrice,
    };

    #[test]
    fn build_reward_snapshot_filters_expired_configs_and_sums_active_daily_rate() {
        let response = RewardMarketResponse {
            data: vec![RewardMarketEntry {
                condition_id: "condition-1".to_string(),
                question: "Will X happen?".to_string(),
                market_slug: "will-x-happen".to_string(),
                event_slug: "will-x-happen".to_string(),
                rewards_max_spread: 3,
                rewards_min_size: 50,
                tokens: vec![
                    RewardTokenPrice {
                        token_id: "yes-token".to_string(),
                        outcome: "YES".to_string(),
                        price: 0.61,
                    },
                    RewardTokenPrice {
                        token_id: "no-token".to_string(),
                        outcome: "NO".to_string(),
                        price: 0.39,
                    },
                ],
                rewards_config: vec![
                    RewardConfigEntry {
                        start_date: NaiveDate::from_ymd_opt(2026, 3, 1).unwrap(),
                        end_date: NaiveDate::from_ymd_opt(2026, 4, 1).unwrap(),
                        rate_per_day: 4.5,
                    },
                    RewardConfigEntry {
                        start_date: NaiveDate::from_ymd_opt(2026, 2, 1).unwrap(),
                        end_date: NaiveDate::from_ymd_opt(2026, 2, 28).unwrap(),
                        rate_per_day: 9.0,
                    },
                ],
            }],
        };

        let snapshot =
            build_reward_snapshot(&response, NaiveDate::from_ymd_opt(2026, 3, 23).unwrap())
                .expect("active reward snapshot");

        assert_eq!(snapshot.condition_id, "condition-1");
        assert_eq!(snapshot.max_spread, "0.03".parse().unwrap());
        assert_eq!(snapshot.min_size, "50".parse().unwrap());
        assert_eq!(snapshot.total_daily_rate, "4.5".parse().unwrap());
        assert_eq!(snapshot.token_prices.len(), 2);
    }

    #[test]
    fn build_reward_snapshot_returns_none_when_no_active_configs_exist() {
        let response = RewardMarketResponse {
            data: vec![RewardMarketEntry {
                condition_id: "condition-1".to_string(),
                question: "Will X happen?".to_string(),
                market_slug: "will-x-happen".to_string(),
                event_slug: "will-x-happen".to_string(),
                rewards_max_spread: 3,
                rewards_min_size: 50,
                tokens: vec![],
                rewards_config: vec![RewardConfigEntry {
                    start_date: NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
                    end_date: NaiveDate::from_ymd_opt(2026, 1, 31).unwrap(),
                    rate_per_day: 4.5,
                }],
            }],
        };

        let snapshot =
            build_reward_snapshot(&response, NaiveDate::from_ymd_opt(2026, 3, 23).unwrap());

        assert!(snapshot.is_none());
    }

    #[tokio::test]
    async fn retry_with_backoff_retries_transient_failures_before_success() {
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = attempts.clone();

        let result = retry_with_backoff(3, std::time::Duration::from_millis(1), move || {
            let observed = observed.clone();
            async move {
                let current = observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                if current < 3 {
                    anyhow::bail!("temporary failure");
                }
                Ok::<_, anyhow::Error>("ok")
            }
        })
        .await
        .expect("retry success");

        assert_eq!(result, "ok");
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[test]
    fn reward_response_json_parses_successfully() {
        let body = r#"{
          "data": [
            {
              "condition_id": "condition-1",
              "question": "Will X happen?",
              "market_slug": "will-x-happen",
              "event_slug": "will-x-happen",
              "rewards_max_spread": 3,
              "rewards_min_size": 50,
              "tokens": [
                {
                  "token_id": "asset-yes",
                  "outcome": "YES",
                  "price": 0.61
                }
              ],
              "rewards_config": [
                {
                  "start_date": "2026-03-01",
                  "end_date": "2026-04-01",
                  "rate_per_day": 4.5
                }
              ]
            }
          ]
        }"#;
        let response = parse_reward_market_response(body).expect("reward JSON parses");
        let snapshot =
            build_reward_snapshot(&response, NaiveDate::from_ymd_opt(2026, 3, 23).unwrap())
                .expect("active reward snapshot");

        assert_eq!(snapshot.condition_id, "condition-1");
        assert_eq!(snapshot.total_daily_rate, "4.5".parse().unwrap());
    }
}
