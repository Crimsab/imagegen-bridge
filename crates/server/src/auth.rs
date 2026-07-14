//! Optional bridge bearer authentication isolated from provider OAuth.

use axum::{
    extract::{Request, State},
    http::{StatusCode, Uri, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use secrecy::{ExposeSecret as _, SecretString};
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;

use crate::{ApiError, RequestId, ServerState};

#[derive(Clone)]
pub(crate) struct AuthPolicy {
    token: SecretString,
    scope: String,
}

impl std::fmt::Debug for AuthPolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthPolicy")
            .field("token", &"[REDACTED]")
            .field("scope", &"[REDACTED]")
            .finish()
    }
}

impl AuthPolicy {
    pub(crate) fn new(token: String) -> Option<Self> {
        if token.is_empty() {
            return None;
        }
        let scope = format!("bearer:{:x}", Sha256::digest(token.as_bytes()));
        Some(Self {
            token: SecretString::from(token),
            scope,
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AuthScope(pub(crate) String);

pub(crate) async fn authorize(
    State(state): State<ServerState>,
    mut request: Request,
    next: Next,
) -> Response {
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .cloned()
        .unwrap_or_else(RequestId::new);
    if !browser_origin_allowed(&request) {
        return ApiError::browser_origin_forbidden(request_id)
            .with_status(StatusCode::FORBIDDEN)
            .into_response();
    }
    let scope = match &state.auth {
        None => "anonymous-local".to_owned(),
        Some(policy) => {
            let supplied = request
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "));
            let valid = supplied.is_some_and(|value| {
                value
                    .as_bytes()
                    .ct_eq(policy.token.expose_secret().as_bytes())
                    .into()
            });
            if !valid {
                return ApiError::authentication(request_id)
                    .with_status(StatusCode::UNAUTHORIZED)
                    .into_response();
            }
            policy.scope.clone()
        }
    };
    request.extensions_mut().insert(AuthScope(scope));
    next.run(request).await
}

fn browser_origin_allowed(request: &Request) -> bool {
    let fetch_sites = request.headers().get_all("sec-fetch-site");
    let mut fetch_sites = fetch_sites.iter();
    if let Some(value) = fetch_sites.next() {
        if fetch_sites.next().is_some() {
            return false;
        }
        let Ok(value) = value.to_str() else {
            return false;
        };
        if !matches!(value, "same-origin" | "none") {
            return false;
        }
    }

    let origins = request.headers().get_all(header::ORIGIN);
    let mut origins = origins.iter();
    let Some(origin) = origins.next() else {
        // Non-browser CLI and SDK requests do not send Origin.
        return true;
    };
    if origins.next().is_some() {
        return false;
    }
    let (Ok(origin), Some(host)) = (
        origin.to_str(),
        request
            .headers()
            .get(header::HOST)
            .and_then(|value| value.to_str().ok()),
    ) else {
        return false;
    };
    let Ok(origin) = origin.parse::<Uri>() else {
        return false;
    };
    let Some(scheme) = origin.scheme_str() else {
        return false;
    };
    if !matches!(scheme, "http" | "https") || origin.query().is_some() {
        return false;
    }
    if !matches!(origin.path(), "" | "/") {
        return false;
    }
    origin
        .authority()
        .is_some_and(|authority| authority.as_str().eq_ignore_ascii_case(host))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use axum::{body::Body, http::Request};

    use super::browser_origin_allowed;

    #[test]
    fn allows_non_browser_and_exact_same_origin_requests() {
        let sdk = Request::get("/v1/providers").body(Body::empty()).unwrap();
        assert!(browser_origin_allowed(&sdk));

        let dashboard = Request::post("/v1/jobs")
            .header("host", "bridge.local:8787")
            .header("origin", "http://bridge.local:8787")
            .header("sec-fetch-site", "same-origin")
            .body(Body::empty())
            .unwrap();
        assert!(browser_origin_allowed(&dashboard));
    }

    #[test]
    fn rejects_cross_origin_and_ambiguous_browser_requests() {
        for request in [
            Request::post("/v1/jobs")
                .header("host", "bridge.local:8787")
                .header("origin", "https://attacker.example")
                .body(Body::empty())
                .unwrap(),
            Request::post("/v1/jobs")
                .header("host", "bridge.local:8787")
                .header("origin", "null")
                .body(Body::empty())
                .unwrap(),
            Request::post("/v1/jobs")
                .header("host", "bridge.local:8787")
                .header("origin", "http://bridge.local:8787/path")
                .body(Body::empty())
                .unwrap(),
            Request::post("/v1/jobs")
                .header("host", "bridge.local:8787")
                .header("sec-fetch-site", "cross-site")
                .body(Body::empty())
                .unwrap(),
            Request::post("/v1/jobs")
                .header("host", "bridge.local:8787")
                .header("sec-fetch-site", "same-site")
                .body(Body::empty())
                .unwrap(),
        ] {
            assert!(!browser_origin_allowed(&request));
        }
    }
}
