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
