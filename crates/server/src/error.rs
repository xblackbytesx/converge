use axum::Json;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("{0}")]
    BadRequest(String),
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    Forbidden(String),
    /// Too many requests of a kind.
    #[error("{0}")]
    Busy(String),
    /// A `/v1` limit (`limits::Refused`): 429 with `Retry-After`.
    #[error("{message}")]
    RateLimited { message: String, retry_after: u64 },
    /// No such model for this key: unknown and not granted look the same.
    #[error("the model `{0}` does not exist or you do not have access to it")]
    ModelNotFound(String),
    /// The upstream could not be reached or broke off. Names the connection,
    /// never its URL.
    #[error("{0}")]
    Upstream(String),
    #[error("{0}")]
    UpstreamTimeout(String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl ApiError {
    pub fn bad(msg: impl Into<String>) -> Self {
        ApiError::BadRequest(msg.into())
    }

    fn parts(&self) -> (StatusCode, String) {
        let (status, msg, _) = self.parts_with_code();
        (status, msg)
    }

    fn parts_with_code(&self) -> (StatusCode, String, Option<&'static str>) {
        let (status, msg) = match self {
            ApiError::ModelNotFound(_) => return (StatusCode::NOT_FOUND, self.to_string(), Some("model_not_found")),
            ApiError::RateLimited { message, .. } => {
                return (StatusCode::TOO_MANY_REQUESTS, message.clone(), Some("rate_limit_exceeded"));
            }
            ApiError::Unauthorized => {
                return (StatusCode::UNAUTHORIZED, "invalid or missing API key".into(), Some("invalid_api_key"));
            }
            ApiError::Upstream(m) => (StatusCode::BAD_GATEWAY, m.clone()),
            ApiError::UpstreamTimeout(m) => (StatusCode::GATEWAY_TIMEOUT, m.clone()),
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m.clone()),
            ApiError::NotFound => (StatusCode::NOT_FOUND, self.to_string()),
            ApiError::Forbidden(m) => (StatusCode::FORBIDDEN, m.clone()),
            ApiError::Busy(m) => (StatusCode::TOO_MANY_REQUESTS, m.clone()),
            ApiError::Db(e) => {
                tracing::error!(error = %e, "database error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string())
            }
            ApiError::Internal(e) => {
                tracing::error!(error = %e, "internal error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string())
            }
        };
        (status, msg, None)
    }
}

/// JSON in OpenAI's error shape, so every client shows Converge's own errors
/// properly (AGENTS.md, Request path).
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let retry_after = match &self {
            ApiError::RateLimited { retry_after, .. } => Some(*retry_after),
            _ => None,
        };
        let (status, msg, code) = self.parts_with_code();
        let kind = match status {
            StatusCode::UNAUTHORIZED => "authentication_error",
            StatusCode::FORBIDDEN => "permission_error",
            StatusCode::NOT_FOUND => "not_found_error",
            StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
            s if s.is_client_error() => "invalid_request_error",
            _ => "server_error",
        };
        let mut r = (status, Json(json!({ "error": { "message": msg, "type": kind, "param": null, "code": code } })))
            .into_response();
        if let Some(s) = retry_after {
            r.headers_mut().insert("retry-after", HeaderValue::from(s));
        }
        r
    }
}

/// The same errors for browser pages: an HTML snippet. HTMX swaps it into the
/// page's `#flash` box (HX-Retarget) instead of the request's own target, so
/// a failed action never replaces a form with an error message.
#[derive(Debug)]
pub struct WebError(pub ApiError);

impl<E: Into<ApiError>> From<E> for WebError {
    fn from(e: E) -> Self {
        WebError(e.into())
    }
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        let (status, msg) = self.0.parts();
        let body = format!("<p class=\"error\" role=\"alert\">{}</p>", escape(&msg));
        let mut r = (status, axum::response::Html(body)).into_response();
        let h = r.headers_mut();
        h.insert("hx-retarget", HeaderValue::from_static("#flash"));
        h.insert("hx-reswap", HeaderValue::from_static("innerHTML"));
        r
    }
}

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}
