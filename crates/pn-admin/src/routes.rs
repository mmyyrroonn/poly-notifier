//! Route definitions for the admin HTTP API.
//!
//! All routes live under the `/admin` prefix and require a valid Bearer token
//! (see [`crate::auth`]).
//!
//! | Method | Path                                   | Handler                         |
//! |--------|----------------------------------------|---------------------------------|
//! | PUT    | `/admin/users/:telegram_id/tier`       | [`handlers::update_tier`]       |
//! | PUT    | `/admin/users/:telegram_id/limit`      | [`handlers::update_limit`]      |
//! | GET    | `/admin/users`                         | [`handlers::list_users`]        |
//! | GET    | `/admin/stats`                         | [`handlers::get_stats`]         |

use axum::{
    middleware,
    routing::{get, post, put},
    Router,
};

use crate::{auth::auth_middleware, handlers, AdminState};

/// Build the admin [`Router`] with authentication applied to every route.
///
/// A single [`AdminState`] is used for both the middleware (to read the
/// admin password) and the handlers (to access the database pool), satisfying
/// axum's requirement that a router has exactly one state type.
pub fn build_router(state: AdminState) -> Router {
    Router::new()
        .route(
            "/admin/users/{telegram_id}/tier",
            put(handlers::update_tier),
        )
        .route(
            "/admin/users/{telegram_id}/limit",
            put(handlers::update_limit),
        )
        .route("/admin/users", get(handlers::list_users))
        .route("/admin/feedback", get(handlers::list_feedback))
        .route("/admin/stats", get(handlers::get_stats))
        .route("/admin/lp/health", get(handlers::lp_health))
        .route("/admin/lp/state", get(handlers::lp_state))
        .route("/admin/lp/pause", post(handlers::lp_pause))
        .route("/admin/lp/resume", post(handlers::lp_resume))
        .route("/admin/lp/cancel-all", post(handlers::lp_cancel_all))
        .route("/admin/lp/flatten", post(handlers::lp_flatten))
        .route("/admin/lp/split", post(handlers::lp_split))
        .route("/admin/lp/merge", post(handlers::lp_merge))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state)
}
