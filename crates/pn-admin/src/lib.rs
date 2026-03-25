//! `pn-admin` — HTTP admin API for poly-notifier.
//!
//! Exposes a small set of authenticated REST endpoints that allow operators to
//! inspect system state and manage user accounts without requiring direct
//! database access.
//!
//! ## Authentication
//!
//! Every request must supply an `Authorization: Bearer <password>` header
//! matching the `admin_password` passed to [`create_router`].  Requests with
//! a missing or incorrect token receive `HTTP 401`.
//!
//! ## Endpoints
//!
//! | Method | Path                                | Description                       |
//! |--------|-------------------------------------|-----------------------------------|
//! | GET    | `/admin/users`                      | List all users with sub counts    |
//! | PUT    | `/admin/users/:telegram_id/tier`    | Update a user's subscription tier |
//! | PUT    | `/admin/users/:telegram_id/limit`   | Update a user's subscription cap  |
//! | GET    | `/admin/stats`                      | System-wide aggregate statistics  |
//!
//! ## Usage
//!
//! ```no_run
//! use sqlx::SqlitePool;
//! use pn_admin::create_router;
//!
//! # async fn example(pool: SqlitePool) {
//! let router = create_router(pool, "s3cr3t".to_string(), None);
//!
//! let listener = tokio::net::TcpListener::bind("0.0.0.0:8080")
//!     .await
//!     .unwrap();
//! axum::serve(listener, router).await.unwrap();
//! # }
//! ```

pub mod auth;
pub mod handlers;
pub mod routes;

use axum::Router;
use pn_lp::MultiLpControlHandle;
use sqlx::SqlitePool;

/// Shared application state threaded through both the auth middleware and
/// all request handlers via axum [`State`](axum::extract::State).
///
/// `Clone` is required by axum to hand out per-request copies.  Both fields
/// are cheap to clone: [`SqlitePool`] is an `Arc`-wrapped pool and
/// [`String`] is heap-allocated but small.
#[derive(Clone)]
pub struct AdminState {
    /// SQLite connection pool used by every handler.
    pub pool: SqlitePool,
    /// Expected admin Bearer token.
    pub admin_password: String,
    /// Optional LP runtime control handle.
    pub lp_control: Option<MultiLpControlHandle>,
}

impl AdminState {
    /// Create a new `AdminState`.
    pub fn new(
        pool: SqlitePool,
        admin_password: String,
        lp_control: Option<MultiLpControlHandle>,
    ) -> Self {
        Self {
            pool,
            admin_password,
            lp_control,
        }
    }
}

/// Construct the admin [`Router`] with all routes and authentication applied.
///
/// Both the auth middleware and the handlers share a single [`AdminState`]
/// value so that axum's single-state-type constraint is satisfied.
///
/// # Arguments
///
/// * `pool` – SQLite connection pool shared with the rest of the application.
/// * `admin_password` – Secret token that callers must supply in the
///   `Authorization: Bearer <token>` header.
pub fn create_router(
    pool: SqlitePool,
    admin_password: String,
    lp_control: Option<MultiLpControlHandle>,
) -> Router {
    let state = AdminState::new(pool, admin_password, lp_control);
    routes::build_router(state)
}
