use crate::defaults;
use axum::{
    body::Body,
    extract::Request,
    http::{header, StatusCode},
    middleware::Next,
    response::Response,
};

/// Enforces `X-Access-Token` once `/init` has provided one. `/health` and
/// `/init` stay reachable so readiness probing and bootstrap keep working.
pub async fn layer(request: Request, next: Next) -> Response {
    let path = request.uri().path();
    if path == "/health" || path == "/init" {
        return next.run(request).await;
    }
    if let Some(expected) = defaults::snapshot().access_token() {
        let provided = request
            .headers()
            .get("x-access-token")
            .and_then(|value| value.to_str().ok());
        if !token_matches(provided, Some(expected.as_str())) {
            return unauthorized();
        }
    }
    next.run(request).await
}

fn token_matches(provided: Option<&str>, expected: Option<&str>) -> bool {
    match expected {
        None => true,
        Some(expected) => provided == Some(expected),
    }
}

fn unauthorized() -> Response {
    let payload = serde_json::json!({
        "code": "unauthenticated",
        "message": "missing or invalid access token",
    });
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(payload.to_string()))
        .expect("valid unauthorized response")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_check_allows_when_unset_and_requires_when_set() {
        assert!(token_matches(None, None));
        assert!(token_matches(Some("x"), None));
        assert!(token_matches(Some("x"), Some("x")));
        assert!(!token_matches(None, Some("x")));
        assert!(!token_matches(Some("y"), Some("x")));
    }

    #[test]
    fn unauthorized_is_401_json() {
        let response = unauthorized();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    }
}
