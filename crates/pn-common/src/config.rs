use config::{Config, Environment, File};
use serde::{Deserialize, Serialize};

use crate::error::Result;

// ---------------------------------------------------------------------------
// Sub-sections
// ---------------------------------------------------------------------------

/// Database connection settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    /// SQLx connection URL, e.g. `sqlite:poly-notifier.db`.
    pub url: String,
    /// Maximum number of connections in the pool.
    pub max_connections: u32,
}

/// Telegram-related settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramConfig {
    /// Maximum messages per minute that may be sent to a single user.
    pub rate_limit_per_user: u32,
}

/// WebSocket monitor settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorConfig {
    /// How often (seconds) the monitor re-fetches the active subscription list.
    pub subscription_refresh_interval_secs: u64,
    /// How often (seconds) a WebSocket ping frame is sent to keep the connection alive.
    pub ws_ping_interval_secs: u64,
    /// Base delay (seconds) before the first reconnect attempt.
    pub reconnect_base_delay_secs: u64,
    /// Upper bound (seconds) for exponential-backoff reconnect delays.
    pub reconnect_max_delay_secs: u64,
}

/// Alert-engine settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertConfig {
    /// How often (seconds) the alert engine refreshes its in-memory alert cache.
    pub cache_refresh_interval_secs: u64,
    /// Default cooldown (minutes) before an alert may fire again.
    pub default_cooldown_minutes: i64,
    /// How often (seconds) in-memory prices are flushed to the database.
    /// Defaults to 30 seconds if omitted.
    #[serde(default = "default_price_flush_interval")]
    pub price_flush_interval_secs: u64,
}

fn default_price_flush_interval() -> u64 {
    30
}

/// Scheduler settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerConfig {
    /// Cron expression for the daily-summary job (6-field: sec min hr day month weekday).
    pub daily_summary_cron: String,
}

/// Admin HTTP server settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminConfig {
    /// TCP port the admin server listens on.
    pub port: u16,
}

/// LP daemon trading/client settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LpTradingConfig {
    /// Polymarket condition ID to quote.
    pub condition_id: String,
    /// CLOB REST base URL.
    #[serde(default = "default_clob_base_url")]
    pub clob_base_url: String,
    /// Gamma REST base URL.
    #[serde(default = "default_gamma_base_url")]
    pub gamma_base_url: String,
    /// Data API base URL.
    #[serde(default = "default_data_base_url")]
    pub data_api_base_url: String,
    /// Polygon chain ID for the authenticated SDK client.
    #[serde(default = "default_polygon_chain_id")]
    pub chain_id: u64,
}

/// LP inventory watermarks and optional split sizing.
///
/// These numeric TOML fields currently deserialize as `f64` and are converted
/// to `Decimal` at runtime via string round-tripping. The expected LP ranges
/// are covered by unit tests in `poly-notifier/src/main.rs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LpInventoryConfig {
    /// Minimum USDC balance required to keep buy quotes live.
    pub min_usdc_balance: f64,
    /// Minimum token inventory required to keep sell quotes live.
    pub min_token_balance: f64,
    /// Whether startup may create inventory via split.
    #[serde(default)]
    pub auto_split_on_startup: bool,
    /// Split amount used when auto_split_on_startup is enabled.
    #[serde(default)]
    pub startup_split_amount: f64,
}

/// Quote-generation parameters.
///
/// These numeric TOML fields currently deserialize as `f64` and are converted
/// to `Decimal` at runtime via string round-tripping. The expected LP ranges
/// are covered by unit tests in `poly-notifier/src/main.rs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LpStrategyConfig {
    /// Quote placement mode: join, inside, or outside.
    #[serde(default = "default_quote_mode")]
    pub quote_mode: String,
    /// Passive quote size per order.
    pub quote_size: f64,
    /// Minimum top-of-book spread required before quoting.
    pub min_spread: f64,
    /// Minimum combined top-level depth required to quote.
    pub min_depth: f64,
    /// How many ticks away from the top of book to quote.
    pub quote_offset_ticks: u32,
    /// Cancel/refresh quotes after this age.
    pub max_quote_age_secs: u64,
    /// Keep the external signal gate on by default until a real feed is added.
    #[serde(default = "default_true")]
    pub default_external_signal: bool,
}

fn default_quote_mode() -> String {
    "inside".to_string()
}

/// Runtime safety controls.
///
/// These numeric TOML fields currently deserialize as `f64` and are converted
/// to `Decimal` at runtime via string round-tripping. The expected LP ranges
/// are covered by unit tests in `poly-notifier/src/main.rs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LpRiskConfig {
    /// Max absolute position before quoting is paused.
    pub max_position: f64,
    /// Position tolerance treated as flat after reconciliation.
    pub flat_position_tolerance: f64,
    /// Automatically flatten after fills.
    #[serde(default = "default_true")]
    pub auto_flatten_after_fill: bool,
    /// Use FOK for flatten orders instead of FAK.
    #[serde(default)]
    pub flatten_use_fok: bool,
    /// Trigger stale-feed risk if no market data arrives within this window.
    pub stale_feed_after_secs: u64,
}

/// Startup approval checks and optional auto-approval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LpApprovalConfig {
    /// Refuse to start if required approvals are missing.
    #[serde(default = "default_true")]
    pub require_on_startup: bool,
    /// Automatically submit approval transactions before bootstrapping.
    #[serde(default)]
    pub auto_approve_on_startup: bool,
}

/// Reporting and operator chat configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LpReportingConfig {
    /// Optional explicit bot ID for operator notifications.
    pub operator_bot_id: Option<String>,
    /// Telegram chat IDs that receive reports and risk alerts.
    #[serde(default)]
    pub operator_chat_ids: Vec<i64>,
    /// Periodic summary interval.
    pub summary_interval_secs: u64,
}

/// Control plane and background task intervals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LpControlConfig {
    /// Hostname/interface for the admin/control server.
    #[serde(default = "default_control_bind_addr")]
    pub bind_addr: String,
    /// Heartbeat submission interval.
    pub heartbeat_interval_secs: u64,
    /// Reconciliation interval for open orders, balances, and positions.
    pub reconciliation_interval_secs: u64,
}

/// Logging and snapshot cadence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LpLoggingConfig {
    /// Persist a state snapshot on this interval even if no report is sent.
    pub snapshot_interval_secs: u64,
    /// Directory for rolling runtime logs.
    #[serde(default = "default_log_directory")]
    pub directory: String,
    /// Prefix used for rolling log files.
    #[serde(default = "default_log_file_prefix")]
    pub file_prefix: String,
    /// Number of rotated log files to retain.
    #[serde(default = "default_log_file_count")]
    pub max_files: usize,
    /// Whether file logs should use JSON formatting.
    #[serde(default = "default_true")]
    pub json: bool,
}

/// Top-level LP daemon configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LpConfig {
    pub trading: LpTradingConfig,
    pub inventory: LpInventoryConfig,
    pub strategy: LpStrategyConfig,
    pub risk: LpRiskConfig,
    #[serde(default)]
    pub approvals: LpApprovalConfig,
    pub reporting: LpReportingConfig,
    pub control: LpControlConfig,
    pub logging: LpLoggingConfig,
}

// ---------------------------------------------------------------------------
// Root config
// ---------------------------------------------------------------------------

/// Complete application configuration loaded from `config/default.toml` and
/// overridden by environment variables prefixed with `APP__` (double
/// underscore as separator so nested keys work, e.g.
/// `APP__DATABASE__URL=sqlite:/tmp/test.db`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub database: DatabaseConfig,
    pub telegram: TelegramConfig,
    pub monitor: MonitorConfig,
    pub alert: AlertConfig,
    pub scheduler: SchedulerConfig,
    pub admin: AdminConfig,
    pub lp: LpConfig,
}

impl AppConfig {
    /// Load configuration from `config/default.toml` (relative to the
    /// current working directory) and overlay any `APP__*` environment
    /// variables.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use pn_common::config::AppConfig;
    ///
    /// let cfg = AppConfig::load().expect("failed to load config");
    /// println!("db url: {}", cfg.database.url);
    /// ```
    pub fn load() -> Result<Self> {
        let cfg = Config::builder()
            .add_source(File::with_name("config/default").required(true))
            .add_source(
                Environment::with_prefix("APP")
                    .separator("__")
                    .ignore_empty(true),
            )
            .build()?;

        let app_config = cfg.try_deserialize::<AppConfig>()?;
        Ok(app_config)
    }

    /// Load configuration from an explicit file path, then overlay env vars.
    /// Useful in tests or alternative deployment layouts.
    pub fn load_from(path: &str) -> Result<Self> {
        let cfg = Config::builder()
            .add_source(File::with_name(path).required(true))
            .add_source(
                Environment::with_prefix("APP")
                    .separator("__")
                    .ignore_empty(true),
            )
            .build()?;

        let app_config = cfg.try_deserialize::<AppConfig>()?;
        Ok(app_config)
    }
}

fn default_clob_base_url() -> String {
    "https://clob.polymarket.com".to_string()
}

fn default_gamma_base_url() -> String {
    "https://gamma-api.polymarket.com".to_string()
}

fn default_data_base_url() -> String {
    "https://data-api.polymarket.com".to_string()
}

fn default_polygon_chain_id() -> u64 {
    137
}

fn default_control_bind_addr() -> String {
    "127.0.0.1".to_string()
}

fn default_log_directory() -> String {
    "logs".to_string()
}

fn default_log_file_prefix() -> String {
    "poly-lp".to_string()
}

fn default_log_file_count() -> usize {
    14
}

fn default_true() -> bool {
    true
}

impl Default for LpApprovalConfig {
    fn default() -> Self {
        Self {
            require_on_startup: true,
            auto_approve_on_startup: false,
        }
    }
}
