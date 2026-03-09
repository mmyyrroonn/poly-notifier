//! Admin HTTP request handlers.
//!
//! Each handler extracts the database pool from [`AdminState`] via axum
//! [`State`], performs the requested database operation, and returns a JSON
//! response.  Database errors are mapped to appropriate HTTP status codes:
//!
//! * `404 Not Found` — the target user does not exist.
//! * `422 Unprocessable Entity` — the request body contained an invalid value.
//! * `500 Internal Server Error` — unexpected database error.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::error;

use crate::AdminState;

// ---------------------------------------------------------------------------
// Error helper
// ---------------------------------------------------------------------------

/// Convert a sqlx error into an axum response, logging it at error level.
fn db_error(e: sqlx::Error) -> Response {
    error!(error = %e, "database error in admin handler");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": "internal database error"})),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// PUT /admin/users/:telegram_id/tier
// ---------------------------------------------------------------------------

/// Request body for the tier-update endpoint.
#[derive(Debug, Deserialize)]
pub struct UpdateTierBody {
    /// Target tier: `"free"`, `"premium"`, or `"unlimited"`.
    pub tier: String,
}

/// Response returned by the tier-update endpoint.
#[derive(Debug, Serialize)]
pub struct UpdateTierResponse {
    pub telegram_id: i64,
    pub tier: String,
}

/// `PUT /admin/users/:telegram_id/tier` — update a user's subscription tier.
///
/// Returns 404 if no user with the given `telegram_id` exists, or 422 if the
/// tier value is not one of `free`, `premium`, `unlimited`.
pub async fn update_tier(
    State(state): State<AdminState>,
    Path(telegram_id): Path<i64>,
    Json(body): Json<UpdateTierBody>,
) -> Response {
    let tier = body.tier.to_lowercase();
    if !matches!(tier.as_str(), "free" | "premium" | "unlimited") {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": format!(
                    "invalid tier {:?}; must be one of: free, premium, unlimited",
                    tier
                )
            })),
        )
            .into_response();
    }

    let now = Utc::now().naive_utc();

    let result =
        sqlx::query("UPDATE users SET tier = ?, updated_at = ? WHERE telegram_id = ?")
            .bind(&tier)
            .bind(now)
            .bind(telegram_id)
            .execute(&state.pool)
            .await;

    match result {
        Err(e) => db_error(e),
        Ok(r) if r.rows_affected() == 0 => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!("user with telegram_id {} not found", telegram_id)
            })),
        )
            .into_response(),
        Ok(_) => Json(UpdateTierResponse { telegram_id, tier }).into_response(),
    }
}

// ---------------------------------------------------------------------------
// PUT /admin/users/:telegram_id/limit
// ---------------------------------------------------------------------------

/// Request body for the subscription-limit update endpoint.
#[derive(Debug, Deserialize)]
pub struct UpdateLimitBody {
    /// New maximum number of active subscriptions for this user.
    pub max_subscriptions: i32,
}

/// Response returned by the limit-update endpoint.
#[derive(Debug, Serialize)]
pub struct UpdateLimitResponse {
    pub telegram_id: i64,
    pub max_subscriptions: i32,
}

/// `PUT /admin/users/:telegram_id/limit` — update a user's subscription cap.
///
/// Returns 404 if no user with the given `telegram_id` exists, or 422 if
/// `max_subscriptions` is negative.
pub async fn update_limit(
    State(state): State<AdminState>,
    Path(telegram_id): Path<i64>,
    Json(body): Json<UpdateLimitBody>,
) -> Response {
    if body.max_subscriptions < 0 {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "max_subscriptions must be non-negative"
            })),
        )
            .into_response();
    }

    let now = Utc::now().naive_utc();

    let result = sqlx::query(
        "UPDATE users SET max_subscriptions = ?, updated_at = ? WHERE telegram_id = ?",
    )
    .bind(body.max_subscriptions)
    .bind(now)
    .bind(telegram_id)
    .execute(&state.pool)
    .await;

    match result {
        Err(e) => db_error(e),
        Ok(r) if r.rows_affected() == 0 => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!("user with telegram_id {} not found", telegram_id)
            })),
        )
            .into_response(),
        Ok(_) => Json(UpdateLimitResponse {
            telegram_id,
            max_subscriptions: body.max_subscriptions,
        })
        .into_response(),
    }
}

// ---------------------------------------------------------------------------
// GET /admin/users
// ---------------------------------------------------------------------------

/// Single entry in the user list response.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct UserListEntry {
    pub id: i64,
    pub telegram_id: i64,
    pub bot_id: String,
    pub username: Option<String>,
    pub tier: String,
    pub max_subscriptions: i32,
    pub timezone: String,
    /// Number of currently active subscriptions for this user.
    pub subscription_count: i64,
}

/// `GET /admin/users` — list all registered users with their active
/// subscription counts.
pub async fn list_users(State(state): State<AdminState>) -> Response {
    let result = sqlx::query_as::<_, UserListEntry>(
        "SELECT
            u.id,
            u.telegram_id,
            u.bot_id,
            u.username,
            u.tier,
            u.max_subscriptions,
            u.timezone,
            COUNT(s.id) AS subscription_count
         FROM users u
         LEFT JOIN subscriptions s
                ON s.user_id = u.id AND s.is_active = 1
         GROUP BY u.id
         ORDER BY u.id",
    )
    .fetch_all(&state.pool)
    .await;

    match result {
        Err(e) => db_error(e),
        Ok(users) => Json(users).into_response(),
    }
}

// ---------------------------------------------------------------------------
// GET /admin/stats
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// GET /admin/feedback
// ---------------------------------------------------------------------------

/// Single entry in the feedback list response.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct FeedbackListEntry {
    pub id: i64,
    pub user_id: i64,
    pub telegram_id: i64,
    pub username: Option<String>,
    pub message: String,
    pub created_at: String,
}

/// `GET /admin/feedback` — list all user feedback, newest first.
pub async fn list_feedback(State(state): State<AdminState>) -> Response {
    let result = sqlx::query_as::<_, FeedbackListEntry>(
        "SELECT
            f.id,
            f.user_id,
            u.telegram_id,
            u.username,
            f.message,
            strftime('%Y-%m-%dT%H:%M:%SZ', f.created_at) AS created_at
         FROM feedback f
         JOIN users u ON u.id = f.user_id
         ORDER BY f.created_at DESC",
    )
    .fetch_all(&state.pool)
    .await;

    match result {
        Err(e) => db_error(e),
        Ok(entries) => Json(entries).into_response(),
    }
}

// ---------------------------------------------------------------------------
// GET /admin/stats
// ---------------------------------------------------------------------------

/// System-wide statistics returned by the stats endpoint.
#[derive(Debug, Serialize)]
pub struct StatsResponse {
    pub total_users: i64,
    pub active_subscriptions: i64,
    pub active_alerts: i64,
    pub active_markets: i64,
    pub notifications_today: i64,
}

/// Row type for scalar aggregate queries — maps `COUNT(*) AS cnt`.
#[derive(sqlx::FromRow)]
struct CountRow {
    cnt: i64,
}

/// `GET /admin/stats` — return aggregate system statistics.
pub async fn get_stats(State(state): State<AdminState>) -> Response {
    match fetch_stats(&state).await {
        Err(e) => db_error(e),
        Ok(s) => Json(s).into_response(),
    }
}

/// Inner function that executes all aggregate queries sequentially.
///
/// Separated from the handler so each `?` propagates a plain
/// [`sqlx::Error`] that `db_error` can log and convert.
async fn fetch_stats(state: &AdminState) -> Result<StatsResponse, sqlx::Error> {
    let total_users =
        sqlx::query_as::<_, CountRow>("SELECT COUNT(*) AS cnt FROM users")
            .fetch_one(&state.pool)
            .await?
            .cnt;

    let active_subscriptions = sqlx::query_as::<_, CountRow>(
        "SELECT COUNT(*) AS cnt FROM subscriptions WHERE is_active = 1",
    )
    .fetch_one(&state.pool)
    .await?
    .cnt;

    let active_alerts = sqlx::query_as::<_, CountRow>(
        "SELECT COUNT(*) AS cnt
         FROM alerts a
         JOIN subscriptions s ON s.id = a.subscription_id
         WHERE s.is_active = 1",
    )
    .fetch_one(&state.pool)
    .await?
    .cnt;

    let active_markets = sqlx::query_as::<_, CountRow>(
        "SELECT COUNT(*) AS cnt FROM markets WHERE is_active = 1",
    )
    .fetch_one(&state.pool)
    .await?
    .cnt;

    // Notifications recorded since midnight UTC today.
    let notifications_today = sqlx::query_as::<_, CountRow>(
        "SELECT COUNT(*) AS cnt FROM notification_log WHERE created_at >= date('now')",
    )
    .fetch_one(&state.pool)
    .await?
    .cnt;

    Ok(StatsResponse {
        total_users,
        active_subscriptions,
        active_alerts,
        active_markets,
        notifications_today,
    })
}
