//! The metrics port. Needs `CONVERGE_TEST_DATABASE_URL`; skipped when unset.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::upstream::{UPSTREAM_KEY, Upstream};
use common::{Db, api};
use converge_server::auth::Role;
use http_body_util::BodyExt as _;
use serde_json::json;
use tower::ServiceExt as _;

async fn scrape(app: &axum::Router, token: Option<&str>) -> (StatusCode, String) {
    let mut b = Request::get("/metrics");
    if let Some(t) = token {
        b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let r = app.clone().oneshot(b.body(Body::empty()).unwrap()).await.unwrap();
    let status = r.status();
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn the_metrics_port_counts_requests() {
    let Some(db) = Db::new().await else { return };
    let up = Upstream::start().await;
    let kim = db.person("kim@example.com", Role::User).await;
    let conn = db.connection("local", &up.base, Some(UPSTREAM_KEY)).await;
    db.model(conn, "local/json", "json").await;
    db.model(conn, "local/error", "error").await;
    db.grant(kim, true, &[]).await;
    let key = db.key(kim).await;
    db.limits(kim, Some(2), None, None).await;
    let app = db.app();
    for m in ["local/json", "local/error", "local/json"] {
        api(&app, "POST", "/v1/chat/completions", Some(&key), Some(json!({"model": m, "messages": []}))).await;
    }

    let metrics = converge_server::metrics_router(db.state());
    let (status, t) = scrape(&metrics, None).await;
    assert_eq!(status, StatusCode::OK);
    for line in [
        "converge_requests_total{endpoint=\"chat/completions\",model=\"local/json\",persona=\"\",status=\"2xx\"} 1",
        "converge_requests_total{endpoint=\"chat/completions\",model=\"local/error\",persona=\"\",status=\"4xx\"} 1",
        "converge_requests_total{endpoint=\"chat/completions\",model=\"local/json\",persona=\"\",status=\"limited\"} 1",
        "converge_tokens_total{model=\"local/json\",kind=\"prompt\"} 11",
        "converge_tokens_total{model=\"local/json\",kind=\"cached\"} 2",
        "converge_request_seconds_count{connection=\"local upstream\"} 2",
        "converge_inflight_requests{connection=\"local upstream\"} 0",
        "converge_limited_total{limit=\"rpm\"} 1",
        "converge_usage_dropped_total 0",
    ] {
        assert!(t.contains(line), "missing {line}\n{t}");
    }
    assert!(!t.contains("kim@example.com"), "no person labels");
    let health = metrics.clone().oneshot(Request::get("/healthz").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(health.status(), StatusCode::OK);

    let main = api(&app, "GET", "/metrics", None, None).await;
    assert_eq!(main.status, StatusCode::NOT_FOUND, "never on the public port");

    let locked = converge_server::metrics_router(converge_server::AppState {
        metrics_token: Some("s3cret".into()),
        ..db.state()
    });
    assert_eq!(scrape(&locked, None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(scrape(&locked, Some("wrong")).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(scrape(&locked, Some("s3cret")).await.0, StatusCode::OK);
    db.drop().await;
}
