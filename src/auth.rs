//! Bearer-token auth middleware for the `gt watch` HTTP endpoints.
//!
//! When `GROUNDTRUTH_TOKEN` is set, every route but `/healthz` requires it; unset
//! leaves all routes open. Comparison is constant-time; the scheme keyword is
//! case-insensitive (RFC 7235).

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use constant_time_eq::constant_time_eq;

/// State passed into the auth middleware.
#[derive(Clone, Debug)]
pub struct AuthState {
    /// The expected bearer token. `None` means no auth required.
    pub token: Option<String>,
}

/// True if `header_value` is `Bearer <token>` matching `expected` (constant-time).
///
/// Scheme is case-insensitive; the token must match exactly (no trimming).
/// Missing header or empty token → false.
pub fn check_bearer(header_value: Option<&str>, expected: &str) -> bool {
    let value = match header_value {
        Some(v) => v,
        None => return false,
    };

    // `get(..7)` is panic-free where `split_at(7)` is not: returns None if the
    // value is under 7 bytes or byte 7 isn't a char boundary (e.g. "Bearer€...").
    let Some(scheme) = value.get(..7) else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("bearer ") {
        return false;
    }
    let token = &value[7..];
    if token.is_empty() {
        return false;
    }

    // Constant-time: never `==` on secrets.
    constant_time_eq(token.as_bytes(), expected.as_bytes())
}

/// Axum middleware enforcing bearer-token auth (no-op when no token is configured).
pub async fn auth_middleware(
    axum::extract::State(state): axum::extract::State<AuthState>,
    headers: HeaderMap,
    request: Request<Body>,
    next: Next,
) -> Response {
    let expected = match &state.token {
        None => return next.run(request).await,
        Some(t) => t.as_str(),
    };

    let header_str = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    if check_bearer(header_str, expected) {
        next.run(request).await
    } else {
        reject_unauthorized()
    }
}

/// 401 with `WWW-Authenticate: Bearer` and a JSON body; never echoes the token.
fn reject_unauthorized() -> Response {
    let body = r#"{"error":"unauthorized","message":"Bearer token required"}"#;
    let mut resp = (StatusCode::UNAUTHORIZED, body).into_response();
    let h = resp.headers_mut();
    h.insert(
        axum::http::header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer"),
    );
    h.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resp
}
