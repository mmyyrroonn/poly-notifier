//! Gamma API client for Polymarket market discovery.
//!
//! The Gamma API groups markets under *events*. Each event has a title and
//! slug and contains one or more markets, each with its own condition ID,
//! question, outcomes, and CLOB token IDs.
//!
//! # Example
//!
//! ```no_run
//! use pn_polymarket::gamma::GammaClient;
//!
//! # #[tokio::main]
//! # async fn main() -> anyhow::Result<()> {
//! let client = GammaClient::new();
//!
//! // Search for active markets matching a keyword.
//! let markets = client.search_markets("election").await?;
//! for m in &markets {
//!     println!("  {} – {} tokens", m.condition_id, m.tokens.len());
//! }
//!
//! // Retrieve a specific market by condition ID.
//! if let Some(market) = client.get_market("0xabc123").await? {
//!     println!("Found: {}", market.question);
//! }
//! # Ok(())
//! # }
//! ```

use anyhow::{Context, Result};
use reqwest::Client;
use tracing::{debug, instrument, warn};

use crate::types::{
    GammaClobReward, GammaEvent, GammaMarket, GammaMarketSummary, GammaRawClobReward,
    GammaRawMarket, GammaRawMarketSummary, GammaToken,
};

const DEFAULT_BASE_URL: &str = "https://gamma-api.polymarket.com";

/// HTTP client for the Polymarket Gamma REST API.
#[derive(Clone)]
pub struct GammaClient {
    http: Client,
    base_url: String,
}

impl GammaClient {
    /// Create a new [`GammaClient`] pointing at the production Gamma API.
    pub fn new() -> Self {
        Self {
            http: Client::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }

    /// Create a client with a custom base URL (useful for testing).
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            http: Client::new(),
            base_url: base_url.into(),
        }
    }

    /// Search for active markets whose event title contains `query`.
    ///
    /// Queries `GET /events?closed=false&limit=10&title={query}` and returns
    /// a flattened list of all markets contained in the matching events.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP request fails or the response body cannot
    /// be parsed as JSON.
    #[instrument(skip(self), fields(query))]
    pub async fn search_markets(&self, query: &str) -> Result<Vec<GammaMarket>> {
        let url = format!("{}/events", self.base_url);
        debug!(%url, %query, "searching Gamma markets");

        let response = self
            .http
            .get(&url)
            .query(&[("closed", "false"), ("limit", "10"), ("title", query)])
            .send()
            .await
            .with_context(|| format!("GET {url} failed"))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .with_context(|| "reading Gamma search response body")?;

        if !status.is_success() {
            anyhow::bail!("Gamma API returned {status} for search query '{query}': {body}");
        }

        let events: Vec<GammaEvent> =
            serde_json::from_str(&body).with_context(|| "parsing Gamma events JSON")?;

        let markets = events.into_iter().flat_map(flatten_event).collect();

        Ok(markets)
    }

    /// Look up markets by the event slug (the URL path segment).
    ///
    /// Queries `GET /events?slug={slug}` and returns a flattened list of all
    /// markets in the matching event.  If the slug lookup yields no results,
    /// falls back to a title search via [`search_markets`].
    #[instrument(skip(self), fields(slug))]
    pub async fn get_market_by_slug(&self, slug: &str) -> Result<Vec<GammaMarket>> {
        let url = format!("{}/events", self.base_url);
        debug!(%url, %slug, "fetching Gamma markets by slug");

        let response = self
            .http
            .get(&url)
            .query(&[("slug", slug)])
            .send()
            .await
            .with_context(|| format!("GET {url} failed"))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .with_context(|| "reading Gamma slug response body")?;

        if !status.is_success() {
            anyhow::bail!("Gamma API returned {status} for slug '{slug}': {body}");
        }

        let events: Vec<GammaEvent> =
            serde_json::from_str(&body).with_context(|| "parsing Gamma events JSON")?;

        let markets: Vec<GammaMarket> = events.into_iter().flat_map(flatten_event).collect();

        if markets.is_empty() {
            debug!(%slug, "slug lookup returned no markets, falling back to title search");
            return self.search_markets(slug).await;
        }

        Ok(markets)
    }

    /// Retrieve a single market by its `condition_id`.
    ///
    /// Queries `GET /events?condition_id={condition_id}` and returns the first
    /// matching market, or `None` if no match is found.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP request fails or the response body cannot
    /// be parsed as JSON.
    #[instrument(skip(self))]
    pub async fn get_market(&self, condition_id: &str) -> Result<Option<GammaMarket>> {
        let url = format!("{}/events", self.base_url);
        debug!(%url, %condition_id, "fetching Gamma market by condition_id");

        let response = self
            .http
            .get(&url)
            .query(&[("condition_id", condition_id)])
            .send()
            .await
            .with_context(|| format!("GET {url} failed"))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .with_context(|| "reading Gamma get_market response body")?;

        if !status.is_success() {
            anyhow::bail!("Gamma API returned {status} for condition_id '{condition_id}': {body}");
        }

        let events: Vec<GammaEvent> =
            serde_json::from_str(&body).with_context(|| "parsing Gamma events JSON")?;

        let market = events
            .into_iter()
            .flat_map(flatten_event)
            .find(|m| m.condition_id == condition_id);

        Ok(market)
    }

    /// Retrieve one page of reward-enriched market summaries from `GET /markets`.
    #[instrument(skip(self), fields(limit, offset, active, closed))]
    pub async fn list_markets_page(
        &self,
        active: bool,
        closed: bool,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<GammaMarketSummary>> {
        let url = format!("{}/markets", self.base_url);
        debug!(%url, limit, offset, active, closed, "fetching Gamma market summaries");

        let response = self
            .http
            .get(&url)
            .query(&[
                ("active", active.to_string()),
                ("closed", closed.to_string()),
                ("limit", limit.to_string()),
                ("offset", offset.to_string()),
            ])
            .send()
            .await
            .with_context(|| format!("GET {url} failed"))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .with_context(|| "reading Gamma markets response body")?;

        if !status.is_success() {
            anyhow::bail!(
                "Gamma API returned {status} for /markets active={active} closed={closed} limit={limit} offset={offset}: {body}"
            );
        }

        parse_market_summaries(&body)
    }
}

impl Default for GammaClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::parse_market_summaries;

    #[test]
    fn market_summaries_parse_reward_fields_and_yes_no_tokens() {
        let body = r#"[
          {
            "id": "531202",
            "question": "BitBoy convicted?",
            "conditionId": "0xb48621f7eba07b0a3eeabc6afb09ae42490239903997b9d412b0f69aeb040c8b",
            "slug": "bitboy-convicted",
            "outcomes": "[\"Yes\", \"No\"]",
            "clobTokenIds": "[\"yes-token\", \"no-token\"]",
            "active": true,
            "closed": false,
            "bestBid": 0.046,
            "bestAsk": 0.048,
            "spread": 0.002,
            "liquidityClob": 6087.35705,
            "volume24hrClob": 7942.105384999998,
            "competitive": 0.8297316067171752,
            "clobRewards": [
              {
                "id": "103219",
                "conditionId": "0xb48621f7eba07b0a3eeabc6afb09ae42490239903997b9d412b0f69aeb040c8b",
                "assetAddress": "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174",
                "rewardsAmount": 0,
                "rewardsDailyRate": 0.001,
                "startDate": "2026-03-15",
                "endDate": "2500-12-31"
              }
            ],
            "rewardsMinSize": 20,
            "rewardsMaxSpread": 3.5
          }
        ]"#;

        let markets = parse_market_summaries(body).expect("market summaries parse");

        assert_eq!(markets.len(), 1);
        let market = &markets[0];
        assert_eq!(
            market.condition_id,
            "0xb48621f7eba07b0a3eeabc6afb09ae42490239903997b9d412b0f69aeb040c8b"
        );
        assert_eq!(market.tokens.len(), 2);
        assert_eq!(market.tokens[0].token_id, "yes-token");
        assert_eq!(market.tokens[0].outcome, "Yes");
        assert_eq!(market.tokens[1].token_id, "no-token");
        assert_eq!(market.best_bid, Some(dec!(0.046)));
        assert_eq!(market.best_ask, Some(dec!(0.048)));
        assert_eq!(market.spread, Some(dec!(0.002)));
        assert_eq!(market.rewards_min_size, Some(dec!(20)));
        assert_eq!(market.rewards_max_spread, Some(dec!(3.5)));
        assert_eq!(market.clob_rewards.len(), 1);
        assert_eq!(market.clob_rewards[0].daily_rate, dec!(0.001));
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert a raw [`GammaEvent`] into zero or more [`GammaMarket`]s.
///
/// Markets with missing mandatory fields are skipped with a warning rather
/// than aborting the whole response.
fn flatten_event(event: GammaEvent) -> Vec<GammaMarket> {
    let event_slug = event.slug.unwrap_or_default();
    let markets_raw = match event.markets {
        Some(m) if !m.is_empty() => m,
        _ => return vec![],
    };

    markets_raw
        .into_iter()
        .filter_map(|raw| parse_raw_market(raw, &event_slug))
        .collect()
}

/// Parse a [`GammaRawMarket`] into a [`GammaMarket`], returning `None` and
/// logging a warning when mandatory fields are absent.
fn parse_raw_market(raw: GammaRawMarket, event_slug: &str) -> Option<GammaMarket> {
    let condition_id = match raw.condition_id {
        Some(id) if !id.is_empty() => id,
        _ => {
            warn!("skipping market with missing condition_id");
            return None;
        }
    };

    let question = raw.question.unwrap_or_default();
    let slug = raw.slug.unwrap_or_else(|| event_slug.to_string());
    let active = raw.active.unwrap_or(false);

    // outcomes is a JSON-encoded string like "[\"Yes\",\"No\"]"
    let outcomes_str = raw.outcomes.unwrap_or_else(|| "[]".to_string());
    let outcome_labels: Vec<String> = serde_json::from_str(&outcomes_str).unwrap_or_else(|e| {
        warn!(%condition_id, error=%e, "failed to parse outcomes JSON, using empty list");
        vec![]
    });

    // clob_token_ids is a JSON-encoded string like "[\"id1\",\"id2\"]"
    let token_ids_str = raw.clob_token_ids.unwrap_or_else(|| "[]".to_string());
    let token_ids: Vec<String> = serde_json::from_str(&token_ids_str).unwrap_or_else(|e| {
        warn!(%condition_id, error=%e, "failed to parse clobTokenIds JSON, using empty list");
        vec![]
    });

    // Zip outcomes and token IDs together. Mismatched lengths are handled
    // gracefully by zip stopping at the shorter iterator.
    let tokens: Vec<GammaToken> = outcome_labels
        .into_iter()
        .zip(token_ids)
        .map(|(outcome, token_id)| GammaToken { token_id, outcome })
        .collect();

    Some(GammaMarket {
        condition_id,
        question,
        slug,
        outcomes: outcomes_str,
        tokens,
        active,
    })
}

fn parse_market_summaries(body: &str) -> Result<Vec<GammaMarketSummary>> {
    let raw_markets: Vec<GammaRawMarketSummary> =
        serde_json::from_str(body).with_context(|| "parsing Gamma markets JSON")?;

    Ok(raw_markets
        .into_iter()
        .filter_map(parse_raw_market_summary)
        .collect())
}

fn parse_raw_market_summary(raw: GammaRawMarketSummary) -> Option<GammaMarketSummary> {
    let condition_id = match raw.condition_id {
        Some(id) if !id.is_empty() => id,
        _ => {
            warn!("skipping market summary with missing condition_id");
            return None;
        }
    };

    let outcomes_str = raw.outcomes.unwrap_or_else(|| "[]".to_string());
    let outcome_labels: Vec<String> = serde_json::from_str(&outcomes_str).unwrap_or_else(|error| {
        warn!(%condition_id, error=%error, "failed to parse outcomes JSON, using empty list");
        vec![]
    });

    let token_ids_str = raw.clob_token_ids.unwrap_or_else(|| "[]".to_string());
    let token_ids: Vec<String> = serde_json::from_str(&token_ids_str).unwrap_or_else(|error| {
        warn!(%condition_id, error=%error, "failed to parse clobTokenIds JSON, using empty list");
        vec![]
    });

    let tokens = outcome_labels
        .into_iter()
        .zip(token_ids)
        .map(|(outcome, token_id)| GammaToken { token_id, outcome })
        .collect();

    let clob_rewards = raw
        .clob_rewards
        .unwrap_or_default()
        .into_iter()
        .filter_map(|reward| parse_clob_reward(reward, &condition_id))
        .collect();

    Some(GammaMarketSummary {
        condition_id,
        question: raw.question.unwrap_or_default(),
        slug: raw.slug.unwrap_or_default(),
        tokens,
        active: raw.active.unwrap_or(false),
        closed: raw.closed.unwrap_or(false),
        best_bid: raw.best_bid,
        best_ask: raw.best_ask,
        spread: raw.spread,
        liquidity_clob: raw.liquidity_clob,
        volume_24hr_clob: raw.volume_24hr_clob,
        competitive: raw.competitive,
        rewards_min_size: raw.rewards_min_size,
        rewards_max_spread: raw.rewards_max_spread,
        clob_rewards,
    })
}

fn parse_clob_reward(
    raw: GammaRawClobReward,
    fallback_condition_id: &str,
) -> Option<GammaClobReward> {
    let id = match raw.id {
        Some(id) if !id.is_empty() => id,
        _ => {
            warn!(
                condition_id = fallback_condition_id,
                "skipping reward summary with missing id"
            );
            return None;
        }
    };

    Some(GammaClobReward {
        id,
        condition_id: raw
            .condition_id
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| fallback_condition_id.to_string()),
        daily_rate: raw.rewards_daily_rate,
        start_date: raw.start_date,
        end_date: raw.end_date,
    })
}
