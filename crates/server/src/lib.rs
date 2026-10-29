//! Converge server library: the router and stores, split from `main` so
//! integration tests drive the real app.

pub mod auth;
pub mod config;
pub mod connections;
pub mod crypto;
pub mod error;
pub mod keys;
pub mod limits;
pub mod metrics;
pub mod personas;
pub mod proxy;
pub mod snapshot;
pub mod usage;
pub mod users;
pub mod web;

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::get;
use sqlx::PgPool;
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    /// `Secure` flag on the web session cookie (and https in invite links).
    pub cookie_secure: bool,
    /// `CONVERGE_PUBLIC_URL`, for invite links.
    pub public_url: Option<String>,
    /// Trust the right-most `X-Forwarded-For` entry (see `Config`).
    pub behind_proxy: bool,
    /// Login throttling, shared by every request.
    pub logins: Arc<limits::LoginLimiter>,
    /// Connection credentials at rest.
    pub secrets: Arc<crypto::Secrets>,
    /// The one client for every upstream (see `snapshot::upstream_client`).
    pub http: reqwest::Client,
    /// What the request path reads. Call `reload` after every admin write.
    pub gateway: Arc<snapshot::Gateway>,
    pub usage: usage::UsageSink,
    /// The `/v1` limits (`limits::ApiLimiter`); seed it before serving.
    pub limiter: Arc<limits::ApiLimiter>,
    pub metrics: Arc<metrics::Metrics>,
    /// `CONVERGE_METRICS_TOKEN`: when set, `/metrics` wants it as a bearer.
    pub metrics_token: Option<String>,
}

impl AppState {
    /// Starts the usage writer, so it needs a running Tokio runtime. The
    /// snapshot starts empty: call `gateway.reload` before serving.
    pub fn new(pool: PgPool, secret_key: &[u8; 32]) -> Self {
        let secrets = Arc::new(crypto::Secrets::new(secret_key));
        Self {
            usage: usage::UsageSink::start(pool.clone()),
            pool,
            cookie_secure: true,
            public_url: None,
            behind_proxy: false,
            logins: Arc::new(limits::LoginLimiter::default()),
            gateway: Arc::new(snapshot::Gateway::new(secrets.clone())),
            secrets,
            http: snapshot::upstream_client(),
            limiter: Default::default(),
            metrics: Default::default(),
            metrics_token: None,
        }
    }

    /// Snapshot and today's token counts, before serving.
    pub async fn start(&self) -> anyhow::Result<()> {
        self.gateway.reload(&self.pool).await?;
        self.limiter.seed(&self.pool).await
    }

    /// Publishes the current database state to the request path. Admin
    /// handlers call it after every write.
    pub async fn reload(&self) -> Result<(), error::ApiError> {
        self.gateway.reload(&self.pool).await.map_err(error::ApiError::Internal)
    }

    /// This server as invitees reach it.
    pub fn public_base(&self, headers: &HeaderMap) -> String {
        self.public_url.clone().unwrap_or_else(|| web::base_url(headers, self.cookie_secure))
    }
}

/// Compression covers the website only: on `/v1` it would cost time to
/// first byte and CPU for answers the clients rarely need compressed.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .merge(web::routes().layer(CompressionLayer::new()))
        .merge(proxy::routes())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// The internal port (`CONVERGE_METRICS_BIND`): never routed by Traefik.
pub fn metrics_router(state: AppState) -> Router {
    Router::new().route("/metrics", get(metrics)).route("/healthz", get(healthz)).with_state(state)
}

async fn healthz(State(st): State<AppState>) -> Result<&'static str, error::ApiError> {
    sqlx::query("SELECT 1").execute(&st.pool).await?;
    Ok("ok")
}

async fn metrics(State(st): State<AppState>, headers: HeaderMap) -> axum::response::Response {
    use axum::response::IntoResponse;
    if let Some(want) = &st.metrics_token {
        let got = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        // Compared as hashes: equal length, no early exit on the secret.
        if auth::secret_hash(got) != auth::secret_hash(want) {
            return (axum::http::StatusCode::UNAUTHORIZED, "bearer token required").into_response();
        }
    }
    let g = metrics::Gauges {
        usage_dropped: st.usage.dropped(),
        usage_queue: st.usage.queued(),
        snapshot_reloads: st.gateway.load().generation,
    };
    ([(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4")], st.metrics.render(&g)).into_response()
}
