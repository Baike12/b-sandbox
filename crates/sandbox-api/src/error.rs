//! API-layer error type: maps domain [`sandbox_core::Error`] codes to HTTP
//! semantics and a stable JSON envelope.
//!
//! Envelope: `{"error": {"code": "...", "message": "..."}}` — codes come from
//! the core error model and are part of the contract. `Error::Internal`
//! messages never reach the client (they may contain host details); the
//! client sees a fixed "internal error" line while the full message is
//! logged server-side at construction time.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use sandbox_core::Error;

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    /// Client-safe message. For `internal`, construction swaps in a fixed
    /// line and logs the real message to stderr.
    message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}: {}", self.status, self.code, self.message)
    }
}

impl ApiError {
    fn from_domain(err: Error) -> Self {
        let code = err.code();
        let (status, message) = match &err {
            Error::Policy(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            Error::Budget(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            Error::Lifecycle(msg) => (StatusCode::CONFLICT, msg.clone()),
            Error::Backend { message, .. } => (StatusCode::BAD_GATEWAY, message.clone()),
            Error::Internal(full) => {
                tracing::error!(message = %full, "internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_string(),
                )
            }
        };
        Self {
            status,
            code,
            message,
        }
    }
}

impl From<Error> for ApiError {
    fn from(err: Error) -> Self {
        Self::from_domain(err)
    }
}

/// A not-found response for a missing sandbox (registry concern, not a core
/// error — constructed directly).
pub fn sandbox_not_found(id: &str) -> ApiError {
    ApiError {
        status: StatusCode::NOT_FOUND,
        code: "sandbox_not_found",
        message: format!("sandbox {id:?} not found"),
    }
}

/// A 401 for a missing/invalid bearer token (token auth only when
/// configured; localhost + nginx deployments may run open).
pub fn unauthorized() -> ApiError {
    ApiError {
        status: StatusCode::UNAUTHORIZED,
        code: "unauthorized",
        message: "missing or invalid bearer token".into(),
    }
}

/// A 503 when the selected backend is not available on this host. There is
/// never a fallback to a weaker isolation tier.
pub fn backend_unavailable(reason: &str) -> ApiError {
    ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        code: "backend_unavailable",
        message: format!("backend unavailable: {reason}"),
    }
}

/// A 413 for request bodies over the declared limit. The envelope code is
/// registry-stable so clients can branch on it like any other error.
pub fn payload_too_large() -> ApiError {
    ApiError {
        status: StatusCode::PAYLOAD_TOO_LARGE,
        code: "payload_too_large",
        message: "request body exceeds the size limit".into(),
    }
}

/// The 500 the panic-catch layer returns. The code is the same `internal`
/// the domain model uses — one internal-error vocabulary, two entry points.
pub fn internal_panic() -> ApiError {
    ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: "internal",
        message: "internal error".into(),
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({
            "error": {"code": self.code, "message": self.message}
        });
        (self.status, Json(body)).into_response()
    }
}

/// JSON extractor that maps deserialization failures (including unknown
/// fields) into the service error envelope with a 400, instead of axum's
/// default 422 with a plain-text body. The envelope is the API contract.
pub struct ApiJson<T>(pub T);

impl<T> std::ops::Deref for ApiJson<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T, S> axum::extract::FromRequest<S> for ApiJson<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(value)) => Ok(Self(value)),
            Err(rejection) => {
                if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
                    Err(payload_too_large())
                } else {
                    Err(ApiError {
                        status: StatusCode::BAD_REQUEST,
                        code: "policy_validation",
                        message: rejection.body_text(),
                    })
                }
            }
        }
    }
}

/// Query extractor that maps deserialization failures into the service
/// error envelope with a 400, mirroring [`ApiJson`] — axum's default query
/// rejection is plain-text 400, and a client that parses every error as the
/// envelope must not hit a different shape for the two extractors.
pub struct ApiQuery<T>(pub T);

impl<T> std::ops::Deref for ApiQuery<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T, S> axum::extract::FromRequestParts<S> for ApiQuery<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(value)) => Ok(Self(value)),
            Err(rejection) => Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "policy_validation",
                message: rejection.body_text(),
            }),
        }
    }
}

/// Alias for handler results.
pub type ApiResult<T> = std::result::Result<T, ApiError>;

#[cfg(test)]
mod tests {
    use super::*;
    use sandbox_core::error::BackendFailureKind;

    #[test]
    fn core_codes_map_to_http_statuses() {
        let cases: Vec<(Error, StatusCode, &str)> = vec![
            (
                Error::Policy("bad path".into()),
                StatusCode::BAD_REQUEST,
                "policy_validation",
            ),
            (
                Error::Budget("too big".into()),
                StatusCode::BAD_REQUEST,
                "budget_rejected",
            ),
            (
                Error::Lifecycle("not running".into()),
                StatusCode::CONFLICT,
                "invalid_transition",
            ),
            (
                Error::Backend {
                    kind: BackendFailureKind::Spawn,
                    message: "spawn failed".into(),
                },
                StatusCode::BAD_GATEWAY,
                "backend_spawn_failed",
            ),
        ];
        for (err, status, code) in cases {
            let api: ApiError = err.into();
            assert_eq!(api.status, status);
            assert_eq!(api.code, code);
        }
    }

    #[test]
    fn not_found_and_unavailable_have_stable_codes() {
        assert_eq!(sandbox_not_found("sbx-x").code, "sandbox_not_found");
        assert_eq!(backend_unavailable("no bwrap").code, "backend_unavailable");
    }
}
