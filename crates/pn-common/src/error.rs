use thiserror::Error;

/// Top-level error type for the poly-notifier shared library.
#[derive(Debug, Error)]
pub enum Error {
    /// Wraps SQLx database errors (pool creation, query execution, migrations).
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    /// Wraps SQLx migration errors.
    #[error("migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),

    /// Wraps configuration loading errors from the `config` crate.
    #[error("configuration error: {0}")]
    Config(#[from] config::ConfigError),

    /// Wraps JSON serialization/deserialization errors.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// A required entity was not found in the database.
    #[error("not found: {0}")]
    NotFound(String),

    /// A business-logic constraint was violated (e.g. subscription limit reached).
    #[error("constraint violation: {0}")]
    Constraint(String),

    /// Catch-all for other unexpected errors.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Convenience alias used throughout the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;
