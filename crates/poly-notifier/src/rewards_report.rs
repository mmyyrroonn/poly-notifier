use crate::rewards_analyzer::{MarketAnalysis, RewardDistribution};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedMarket {
    pub condition_id: String,
    pub slug: String,
    pub question: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewardsReport {
    pub generated_at: String,
    pub scan_count: usize,
    pub analyzed_count: usize,
    pub skipped_count: usize,
    pub distribution: Option<RewardDistribution>,
    pub analyses: Vec<MarketAnalysis>,
    pub skipped: Vec<SkippedMarket>,
}
