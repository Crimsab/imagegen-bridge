//! Optional bridge bearer authentication isolated from provider OAuth.

use axum::{
    extract::{Request, State},
    http::{StatusCode, header},
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
