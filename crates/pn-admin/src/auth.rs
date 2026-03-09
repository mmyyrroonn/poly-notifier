//! Bearer-token authentication middleware for the admin HTTP server.
//!
//! Every request to an admin route must include an `Authorization: Bearer
//! <password>` header that matches the configured admin password.  Requests
//! that are missing the header or supply the wrong token receive an HTTP 401
//! response with no body.
//!
//! The middleware reads the expected password from the shared [`AdminState`]
//! via axum [`State`] extraction and performs a direct string comparison.
//!
//! # Example
//!
//! ```no_run
//! use axum::{Router, middleware, routing::get};
//! use pn_admin::{auth::auth_middleware, AdminState};
//! use sqlx::SqlitePool;
//!
//! # async fn example(pool: SqlitePool) {
//! let state = AdminState::new(pool, "s3cr3t".to_string());
//! let app: Router = Router::new()
//!     .route("/admin/ping", get(|| async { "pong" }))
//!     .layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
//!     .with_state(state);
//! # }
//! ```

use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::Response,
};

use crate::AdminState;

/// Axum middleware that validates the `Authorization: Bearer <token>` header.
///
/// The expected password is read from [`AdminState::admin_password`].
/// Registered via [`axum::middleware::from_fn_with_state`].
pub async fn auth_middleware(
    State(state): State<AdminState>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let token = extract_bearer_token(&req)?;

    if token != state.admin_password {
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(next.run(req).await)
}

/// Extract the raw token string from `Authorization: Bearer <token>`.
///
/// Returns `Err(401)` if the header is absent, not valid UTF-8, or not a
/// Bearer scheme.
pub(crate) fn extract_bearer_token(req: &Request) -> Result<&str, StatusCode> {
    let header_value = req
        .headers()
        .get(header::AUTHORIZATION)
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let header_str = header_value
        .to_str()
        .map_err(|_| StatusCode::UNAUTHORIZED)?;

    header_str
        .strip_prefix("Bearer ")
        .ok_or(StatusCode::UNAUTHORIZED)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderValue, Method};

    fn make_request(auth_header: Option<&str>) -> Request {
        let mut builder = axum::http::Request::builder()
            .method(Method::GET)
            .uri("/admin/test");
        if let Some(value) = auth_header {
            builder = builder.header(header::AUTHORIZATION, HeaderValue::from_str(value).unwrap());
        }
        builder.body(axum::body::Body::empty()).unwrap()
    }

    #[test]
    fn missing_header_returns_unauthorized() {
        let req = make_request(None);
        assert_eq!(extract_bearer_token(&req), Err(StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn non_bearer_scheme_returns_unauthorized() {
        let req = make_request(Some("Basic dXNlcjpwYXNz"));
        assert_eq!(extract_bearer_token(&req), Err(StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn valid_bearer_token_is_extracted() {
        let req = make_request(Some("Bearer mysecret"));
        assert_eq!(extract_bearer_token(&req), Ok("mysecret"));
    }

    #[test]
    fn empty_bearer_token_is_extracted_as_empty_string() {
        let req = make_request(Some("Bearer "));
        assert_eq!(extract_bearer_token(&req), Ok(""));
    }
}
