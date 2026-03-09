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

use crate::types::{GammaEvent, GammaMarket, GammaRawMarket, GammaToken};

const DEFAULT_BASE_URL: &str = "https://gamma-api.polymarket.com";

/// HTTP client for the Polymarket Gamma REST API.
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
            .query(&[
                ("closed", "false"),
                ("limit", "10"),
                ("title", query),
            ])
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

        let markets = events
            .into_iter()
            .flat_map(flatten_event)
            .collect();

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
            anyhow::bail!(
                "Gamma API returned {status} for condition_id '{condition_id}': {body}"
            );
        }

        let events: Vec<GammaEvent> =
            serde_json::from_str(&body).with_context(|| "parsing Gamma events JSON")?;

        let market = events
            .into_iter()
            .flat_map(flatten_event)
            .find(|m| m.condition_id == condition_id);

        Ok(market)
    }
}

impl Default for GammaClient {
    fn default() -> Self {
        Self::new()
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
