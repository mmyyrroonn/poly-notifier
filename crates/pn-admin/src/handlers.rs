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
use pn_lp::{ControlCommand, RuntimeSnapshot};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
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

fn lp_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "lp runtime is not configured"})),
    )
        .into_response()
}

fn lp_command_error(error: String) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": error })),
    )
        .into_response()
}

#[allow(clippy::result_large_err)]
fn lp_handle(state: &AdminState) -> Result<pn_lp::LpControlHandle, Response> {
    state.lp_control.clone().ok_or_else(lp_unavailable)
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

    let result = sqlx::query("UPDATE users SET tier = ?, updated_at = ? WHERE telegram_id = ?")
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

    let result =
        sqlx::query("UPDATE users SET max_subscriptions = ?, updated_at = ? WHERE telegram_id = ?")
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

// ---------------------------------------------------------------------------
// LP control plane
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
pub struct ReasonBody {
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SignalBody {
    pub name: String,
    pub active: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AmountBody {
    pub amount: String,
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LpHealthResponse {
    pub paused: bool,
    pub flattening: bool,
    pub heartbeat_healthy: bool,
    pub market_feed_healthy: bool,
    pub user_feed_healthy: bool,
    pub open_orders: usize,
    pub positions: usize,
    pub last_market_event_at: Option<String>,
    pub last_user_event_at: Option<String>,
    pub last_heartbeat_at: Option<String>,
    pub last_decision_reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LpCommandResponse {
    pub status: &'static str,
}

pub async fn lp_health(State(state): State<AdminState>) -> Response {
    let handle = match lp_handle(&state) {
        Ok(handle) => handle,
        Err(response) => return response,
    };
    let snapshot = handle.snapshot();
    Json(LpHealthResponse {
        paused: snapshot.flags.paused,
        flattening: snapshot.flags.flattening,
        heartbeat_healthy: snapshot.flags.heartbeat_healthy,
        market_feed_healthy: snapshot.flags.market_feed_healthy,
        user_feed_healthy: snapshot.flags.user_feed_healthy,
        open_orders: snapshot.open_orders.len(),
        positions: snapshot.positions.len(),
        last_market_event_at: snapshot.last_market_event_at.map(|ts| ts.to_rfc3339()),
        last_user_event_at: snapshot.last_user_event_at.map(|ts| ts.to_rfc3339()),
        last_heartbeat_at: snapshot.last_heartbeat_at.map(|ts| ts.to_rfc3339()),
        last_decision_reason: snapshot.last_decision_reason,
    })
    .into_response()
}

pub async fn lp_state(State(state): State<AdminState>) -> Response {
    let handle = match lp_handle(&state) {
        Ok(handle) => handle,
        Err(response) => return response,
    };
    let snapshot: RuntimeSnapshot = handle.snapshot();
    Json(snapshot).into_response()
}

pub async fn lp_pause(State(state): State<AdminState>, body: Option<Json<ReasonBody>>) -> Response {
    let handle = match lp_handle(&state) {
        Ok(handle) => handle,
        Err(response) => return response,
    };
    let reason = body
        .and_then(|body| body.reason.clone())
        .unwrap_or_else(|| "admin pause".to_string());
    match handle.send(ControlCommand::Pause { reason }) {
        Ok(()) => Json(LpCommandResponse { status: "ok" }).into_response(),
        Err(error) => lp_command_error(error),
    }
}

pub async fn lp_resume(
    State(state): State<AdminState>,
    body: Option<Json<ReasonBody>>,
) -> Response {
    let handle = match lp_handle(&state) {
        Ok(handle) => handle,
        Err(response) => return response,
    };
    let reason = body
        .and_then(|body| body.reason.clone())
        .unwrap_or_else(|| "admin resume".to_string());
    match handle.send(ControlCommand::Resume { reason }) {
        Ok(()) => Json(LpCommandResponse { status: "ok" }).into_response(),
        Err(error) => lp_command_error(error),
    }
}

pub async fn lp_cancel_all(
    State(state): State<AdminState>,
    body: Option<Json<ReasonBody>>,
) -> Response {
    let handle = match lp_handle(&state) {
        Ok(handle) => handle,
        Err(response) => return response,
    };
    let reason = body
        .and_then(|body| body.reason.clone())
        .unwrap_or_else(|| "admin cancel-all".to_string());
    match handle.send(ControlCommand::CancelAll { reason }) {
        Ok(()) => Json(LpCommandResponse { status: "ok" }).into_response(),
        Err(error) => lp_command_error(error),
    }
}

pub async fn lp_signal(State(state): State<AdminState>, Json(body): Json<SignalBody>) -> Response {
    let handle = match lp_handle(&state) {
        Ok(handle) => handle,
        Err(response) => return response,
    };
    let reason = body
        .reason
        .unwrap_or_else(|| format!("admin signal {}", body.name));
    match handle.send(ControlCommand::ExternalSignal {
        name: body.name,
        active: body.active,
        reason,
    }) {
        Ok(()) => Json(LpCommandResponse { status: "ok" }).into_response(),
        Err(error) => lp_command_error(error),
    }
}

pub async fn lp_flatten(
    State(state): State<AdminState>,
    body: Option<Json<ReasonBody>>,
) -> Response {
    let handle = match lp_handle(&state) {
        Ok(handle) => handle,
        Err(response) => return response,
    };
    let reason = body
        .and_then(|body| body.reason.clone())
        .unwrap_or_else(|| "admin flatten".to_string());
    match handle.send(ControlCommand::Flatten { reason }) {
        Ok(()) => Json(LpCommandResponse { status: "ok" }).into_response(),
        Err(error) => lp_command_error(error),
    }
}

pub async fn lp_split(State(state): State<AdminState>, body: Option<Json<AmountBody>>) -> Response {
    let handle = match lp_handle(&state) {
        Ok(handle) => handle,
        Err(response) => return response,
    };
    let Some(body) = body else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({"error": "split amount is required"})),
        )
            .into_response();
    };
    let amount = match Decimal::from_str(&body.amount) {
        Ok(amount) if amount > Decimal::ZERO => body.amount.clone(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid or non-positive amount"})),
            )
                .into_response();
        }
    };
    match handle.send(ControlCommand::Split {
        amount,
        reason: body
            .reason
            .clone()
            .unwrap_or_else(|| "admin split".to_string()),
    }) {
        Ok(()) => Json(LpCommandResponse { status: "ok" }).into_response(),
        Err(error) => lp_command_error(error),
    }
}

pub async fn lp_merge(State(state): State<AdminState>, body: Option<Json<AmountBody>>) -> Response {
    let handle = match lp_handle(&state) {
        Ok(handle) => handle,
        Err(response) => return response,
    };
    let Some(body) = body else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({"error": "merge amount is required"})),
        )
            .into_response();
    };
    let amount = match Decimal::from_str(&body.amount) {
        Ok(amount) if amount > Decimal::ZERO => body.amount.clone(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid or non-positive amount"})),
            )
                .into_response();
        }
    };
    match handle.send(ControlCommand::Merge {
        amount,
        reason: body
            .reason
            .clone()
            .unwrap_or_else(|| "admin merge".to_string()),
    }) {
        Ok(()) => Json(LpCommandResponse { status: "ok" }).into_response(),
        Err(error) => lp_command_error(error),
    }
}

/// Inner function that executes all aggregate queries sequentially.
///
/// Separated from the handler so each `?` propagates a plain
/// [`sqlx::Error`] that `db_error` can log and convert.
async fn fetch_stats(state: &AdminState) -> Result<StatsResponse, sqlx::Error> {
    let total_users = sqlx::query_as::<_, CountRow>("SELECT COUNT(*) AS cnt FROM users")
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

    let active_markets =
        sqlx::query_as::<_, CountRow>("SELECT COUNT(*) AS cnt FROM markets WHERE is_active = 1")
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use axum::{
        body::{to_bytes, Body},
        extract::State,
        http::StatusCode,
    };
    use chrono::Utc;
    use pn_lp::types::SignalState;
    use pn_lp::{
        AccountSnapshot, ControlCommand, LpControlHandle, MarketMetadata, RuntimeFlags,
        RuntimeState, TokenMetadata,
    };
    use sqlx::SqlitePool;
    use tokio::sync::{mpsc, watch};

    use super::{lp_merge, lp_signal, lp_split, AmountBody, SignalBody};
    use crate::AdminState;

    async fn test_admin_state() -> (AdminState, mpsc::UnboundedReceiver<ControlCommand>) {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite pool");
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (_snapshot_tx, snapshot_rx) = watch::channel(runtime_state());
        let handle = LpControlHandle::new(cmd_tx, snapshot_rx);
        (
            AdminState::new(pool, "secret".to_string(), Some(handle)),
            cmd_rx,
        )
    }

    fn runtime_state() -> RuntimeState {
        let now = Utc::now();
        RuntimeState {
            market: MarketMetadata {
                condition_id: "condition-1".to_string(),
                question: "Will X happen?".to_string(),
                tokens: vec![TokenMetadata {
                    asset_id: "asset-yes".to_string(),
                    outcome: "Yes".to_string(),
                    tick_size: "0.01".parse().unwrap(),
                }],
            },
            books: HashMap::new(),
            open_orders: Vec::new(),
            positions: HashMap::new(),
            account: AccountSnapshot {
                usdc_balance: "100".parse().unwrap(),
                token_balances: HashMap::new(),
                updated_at: now,
            },
            signals: HashMap::from([(
                "external".to_string(),
                SignalState {
                    active: true,
                    reason: "test".to_string(),
                },
            )]),
            flags: RuntimeFlags::default(),
            last_market_event_at: Some(now),
            last_user_event_at: Some(now),
            last_heartbeat_at: Some(now),
            last_heartbeat_id: Some("hb-1".to_string()),
            last_decision_reason: None,
        }
    }

    async fn response_body_json(body: Body) -> serde_json::Value {
        let bytes = to_bytes(body, usize::MAX)
            .await
            .expect("response body bytes");
        serde_json::from_slice(&bytes).expect("json response body")
    }

    #[tokio::test]
    async fn lp_split_rejects_non_positive_amounts() {
        let (state, mut cmd_rx) = test_admin_state().await;

        let response = lp_split(
            State(state),
            Some(axum::Json(AmountBody {
                amount: "0".to_string(),
                reason: None,
            })),
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response_body_json(response.into_body()).await,
            serde_json::json!({ "error": "invalid or non-positive amount" })
        );
        assert!(cmd_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn lp_merge_rejects_invalid_amounts() {
        let (state, mut cmd_rx) = test_admin_state().await;

        let response = lp_merge(
            State(state),
            Some(axum::Json(AmountBody {
                amount: "nope".to_string(),
                reason: None,
            })),
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response_body_json(response.into_body()).await,
            serde_json::json!({ "error": "invalid or non-positive amount" })
        );
        assert!(cmd_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn lp_signal_dispatches_external_signal_command() {
        let (state, mut cmd_rx) = test_admin_state().await;

        let response = lp_signal(
            State(state),
            axum::Json(SignalBody {
                name: "external".to_string(),
                active: false,
                reason: None,
            }),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        match cmd_rx.try_recv().unwrap() {
            ControlCommand::ExternalSignal {
                name,
                active,
                reason,
            } => {
                assert_eq!(name, "external");
                assert!(!active);
                assert_eq!(reason, "admin signal external");
            }
            other => panic!("expected external signal command, got {other:?}"),
        }
    }
}
