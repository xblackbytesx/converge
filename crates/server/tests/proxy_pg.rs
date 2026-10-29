//! `/v1` against the stand-in upstream. Needs `CONVERGE_TEST_DATABASE_URL`;
//! skipped when unset.

mod common;

use std::time::Duration;

use axum::http::StatusCode;
use common::upstream::{CHAT_JSON, ERROR_JSON, UPSTREAM_KEY, Upstream};
use common::{Db, SECRET, api, api_raw};
use converge_server::auth::Role;
use futures_util::StreamExt;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use uuid::Uuid;

/// A user with one key and every model granted, on one connection with
/// models named after the stand-in's behaviours.
struct Setup {
    db: Db,
    up: Upstream,
    key: String,
    user: Uuid,
    conn: Uuid,
}

async fn setup() -> Option<Setup> {
    let db = Db::new().await?;
    let up = Upstream::start().await;
    let user = db.person("kim@example.com", Role::User).await;
    let headers = vec![("x-title".to_string(), "Converge test".to_string())];
    let conn = db.connection_with("local", &up.base, Some(UPSTREAM_KEY), headers, 1, 1).await;
    for name in
        ["json", "embed", "error", "speech", "stream", "inline-usage", "slow-headers", "stall", "die", "endless"]
    {
        db.model(conn, &format!("local/{name}"), name).await;
    }
    db.grant(user, true, &[]).await;
    let key = db.key(user).await;
    Some(Setup { db, up, key, user, conn })
}

fn chat(model: &str, stream: bool) -> Value {
    json!({"model": model, "stream": stream, "messages": [{"role": "user", "content": "hi"}], "x_vendor": {"keep": 1}})
}

async fn usage_rows(db: &Db) -> Vec<(String, i32, i64, i64, bool, bool)> {
    db.flush().await;
    sqlx::query_as(
        "SELECT model_name, status, prompt_tokens, completion_tokens, usage_reported, client_closed FROM usage ORDER BY id",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn keys_are_checked_and_errors_look_like_openai() {
    let Some(s) = setup().await else { return };
    let app = s.db.app();
    for key in [None, Some("not-a-key"), Some("cvg_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")] {
        let r = api(&app, "POST", "/v1/chat/completions", key, Some(chat("local/json", false))).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{key:?}");
        let v: Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["error"]["type"], "authentication_error");
        assert_eq!(v["error"]["code"], "invalid_api_key");
    }
    assert!(s.up.seen().is_empty(), "nothing reached the upstream");

    let ok = api(&app, "POST", "/v1/chat/completions", Some(&s.key), Some(chat("local/json", false))).await;
    assert_eq!(ok.status, StatusCode::OK);

    // Revoked, expired and disabled all stop the key at once.
    let id: Uuid = sqlx::query_scalar("SELECT id FROM api_keys").fetch_one(&s.db.pool).await.unwrap();
    converge_server::keys::revoke(&s.db.pool, id, None).await.unwrap();
    s.db.reload().await;
    assert_eq!(api(&app, "GET", "/v1/models", Some(&s.key), None).await.status, StatusCode::UNAUTHORIZED);

    let k2 = s.db.key(s.user).await;
    assert_eq!(api(&app, "GET", "/v1/models", Some(&k2), None).await.status, StatusCode::OK);
    sqlx::query("UPDATE api_keys SET expires_at = now() - interval '1 minute' WHERE revoked_at IS NULL")
        .execute(&s.db.pool)
        .await
        .unwrap();
    s.db.reload().await;
    assert_eq!(api(&app, "GET", "/v1/models", Some(&k2), None).await.status, StatusCode::UNAUTHORIZED);

    let k3 = s.db.key(s.user).await;
    sqlx::query("UPDATE users SET disabled_at = now() WHERE id = $1").bind(s.user).execute(&s.db.pool).await.unwrap();
    s.db.reload().await;
    assert_eq!(api(&app, "GET", "/v1/models", Some(&k3), None).await.status, StatusCode::UNAUTHORIZED);
    s.db.drop().await;
}

#[tokio::test]
async fn grants_decide_what_a_key_sees_and_uses() {
    let Some(s) = setup().await else { return };
    let app = s.db.app();
    let sam = s.db.person("sam@example.com", Role::User).await;
    let sam_key = s.db.key(sam).await;

    let r = api(&app, "GET", "/v1/models", Some(&sam_key), None).await;
    assert_eq!(serde_json::from_str::<Value>(&r.body).unwrap()["data"], json!([]));
    let denied = api(&app, "POST", "/v1/chat/completions", Some(&sam_key), Some(chat("local/json", false))).await;
    let unknown = api(&app, "POST", "/v1/chat/completions", Some(&sam_key), Some(chat("local/nope", false))).await;
    assert_eq!(denied.status, StatusCode::NOT_FOUND);
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    let (d, u): (Value, Value) =
        (serde_json::from_str(&denied.body).unwrap(), serde_json::from_str(&unknown.body).unwrap());
    assert_eq!(d["error"]["code"], "model_not_found");
    assert_eq!(d["error"]["code"], u["error"]["code"], "not granted looks like not there");

    let json_model: Uuid =
        sqlx::query_scalar("SELECT id FROM models WHERE public_id = 'local/json'").fetch_one(&s.db.pool).await.unwrap();
    s.db.grant(sam, false, &[json_model]).await;
    let r = api(&app, "GET", "/v1/models", Some(&sam_key), None).await;
    let v: Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(v["object"], "list");
    assert_eq!(v["data"].as_array().unwrap().len(), 1);
    assert_eq!(v["data"][0]["id"], "local/json");
    assert_eq!(v["data"][0]["owned_by"], "local upstream");
    assert_eq!(v["data"][0]["converge"]["kind"], "model");
    let one = api(&app, "GET", "/v1/models/local/json", Some(&sam_key), None).await;
    assert_eq!(one.status, StatusCode::OK, "ids with a slash work");
    assert_eq!(api(&app, "GET", "/v1/models/local/stream", Some(&sam_key), None).await.status, StatusCode::NOT_FOUND);

    // Admins need no grants; a disabled model disappears for everyone.
    let admin = s.db.person("admin@example.com", Role::Admin).await;
    let admin_key = s.db.key(admin).await;
    let all: Value = serde_json::from_str(&api(&app, "GET", "/v1/models", Some(&admin_key), None).await.body).unwrap();
    assert_eq!(all["data"].as_array().unwrap().len(), 10);
    converge_server::connections::set_model_switches(&s.db.pool, json_model, false, true, true).await.unwrap();
    s.db.reload().await;
    let r = api(&app, "POST", "/v1/chat/completions", Some(&sam_key), Some(chat("local/json", false))).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    s.db.drop().await;
}

#[tokio::test]
async fn requests_reach_the_upstream_rewritten_and_nothing_else_travels() {
    let Some(s) = setup().await else { return };
    let app = s.db.app();
    let resp = api_raw(&app, "/v1/chat/completions", &s.key, chat("local/json", false)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["content-type"], "application/json");
    assert!(resp.headers().get("content-encoding").is_none(), "no compression on /v1");
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body, CHAT_JSON.as_bytes(), "byte for byte");

    let seen = s.up.last();
    assert_eq!(seen.path, "/v1/chat/completions");
    assert_eq!(seen.body["model"], "json", "the upstream's own name");
    assert_eq!(seen.body["x_vendor"], json!({"keep": 1}), "unknown fields kept");
    assert!(seen.body.get("stream_options").is_none(), "not a stream: nothing added");
    assert_eq!(seen.headers["authorization"], format!("Bearer {UPSTREAM_KEY}"));
    assert_eq!(seen.headers["x-title"], "Converge test");
    assert!(seen.headers.get("cookie").is_none());
    let all: String = seen.headers.iter().map(|(_, v)| v.to_str().unwrap_or("")).collect();
    assert!(!all.contains(&s.key), "the client's key never travels upstream");

    let rows = usage_rows(&s.db).await;
    assert_eq!(rows, vec![("local/json".into(), 200, 11, 7, true, false)]);
    let cached: i64 = sqlx::query_scalar("SELECT cached_tokens FROM usage").fetch_one(&s.db.pool).await.unwrap();
    assert_eq!(cached, 2);
    let used: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT last_used_at FROM api_keys").fetch_one(&s.db.pool).await.unwrap();
    assert!(used.is_some(), "last_used_at follows the usage writer");

    // Other endpoints go to their own path; JSON usage is read there too.
    let r =
        api(&app, "POST", "/v1/embeddings", Some(&s.key), Some(json!({"model": "local/embed", "input": "x"}))).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(s.up.last().path, "/v1/embeddings");
    let r = api_raw(&app, "/v1/audio/speech", &s.key, json!({"model": "local/speech", "input": "x"})).await;
    assert_eq!(r.headers()["content-type"], "audio/mpeg");
    assert_eq!(r.into_body().collect().await.unwrap().to_bytes().as_ref(), &[0u8, 1, 2, 255]);
    let rows = usage_rows(&s.db).await;
    assert_eq!(rows[1], ("local/embed".into(), 200, 3, 0, true, false));
    assert_eq!(rows[2], ("local/speech".into(), 200, 0, 0, false, false));
    s.db.drop().await;
}

#[tokio::test]
async fn upstream_errors_pass_through_as_they_are() {
    let Some(s) = setup().await else { return };
    let r = api(&s.db.app(), "POST", "/v1/chat/completions", Some(&s.key), Some(chat("local/error", false))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.body, ERROR_JSON);
    assert_eq!(usage_rows(&s.db).await, vec![("local/error".into(), 400, 0, 0, false, false)]);
    s.db.drop().await;
}

#[tokio::test]
async fn streams_get_usage_without_the_client_seeing_the_extra_chunk() {
    let Some(s) = setup().await else { return };
    let app = s.db.app();

    // The client did not ask: Converge asks, reads it, removes it.
    let r = api(&app, "POST", "/v1/chat/completions", Some(&s.key), Some(chat("local/stream", true))).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers["content-type"], "text/event-stream");
    assert_eq!(r.headers["cache-control"], "no-cache");
    assert_eq!(s.up.last().body["stream_options"], json!({"include_usage": true}));
    assert!(!r.body.contains("\"usage\""), "{}", r.body);
    assert!(r.body.contains("Hel") && r.body.ends_with("data: [DONE]\n\n"), "{}", r.body);

    // The client asked: it gets the chunk.
    let mut asked = chat("local/stream", true);
    asked["stream_options"] = json!({"include_usage": true});
    let r = api(&app, "POST", "/v1/chat/completions", Some(&s.key), Some(asked)).await;
    assert!(r.body.contains("\"usage\""));

    // Usage riding on a content chunk is read and left alone.
    let r = api(&app, "POST", "/v1/chat/completions", Some(&s.key), Some(chat("local/inline-usage", true))).await;
    assert!(r.body.contains("\"prompt_tokens\":9"));

    let rows = usage_rows(&s.db).await;
    assert_eq!(rows[0], ("local/stream".into(), 200, 5, 3, true, false));
    assert_eq!(rows[1], ("local/stream".into(), 200, 5, 3, true, false));
    assert_eq!(rows[2], ("local/inline-usage".into(), 200, 9, 1, true, false));
    let ttfb: Option<i32> =
        sqlx::query_scalar("SELECT ttfb_ms FROM usage ORDER BY id LIMIT 1").fetch_one(&s.db.pool).await.unwrap();
    assert!(ttfb.is_some());
    s.db.drop().await;
}

#[tokio::test]
async fn slow_and_broken_upstreams_are_cut_off() {
    let Some(s) = setup().await else { return };
    let app = s.db.app();

    // No headers within first_byte_secs (1 s here): 504, naming the connection.
    let r = api(&app, "POST", "/v1/chat/completions", Some(&s.key), Some(chat("local/slow-headers", true))).await;
    assert_eq!(r.status, StatusCode::GATEWAY_TIMEOUT);
    assert!(r.body.contains("local upstream") && !r.body.contains("127.0.0.1"), "{}", r.body);

    // Silent mid-stream for chunk_secs: the body ends in an error, not short.
    for model in ["local/stall", "local/die"] {
        let resp = api_raw(&app, "/v1/chat/completions", &s.key, chat(model, true)).await;
        if resp.status() != StatusCode::OK {
            let b = resp.into_body().collect().await.unwrap().to_bytes();
            panic!("{model}: {}", String::from_utf8_lossy(&b));
        }
        assert!(resp.into_body().collect().await.is_err(), "{model} must not end like a finished answer");
    }

    // Nothing listens there: 502.
    let dead = s.db.connection("dead", "http://127.0.0.1:9/v1", None).await;
    s.db.model(dead, "dead/x", "x").await;
    let r = api(&app, "POST", "/v1/chat/completions", Some(&s.key), Some(chat("dead/x", false))).await;
    assert_eq!(r.status, StatusCode::BAD_GATEWAY);
    assert!(r.body.contains("could not connect"), "{}", r.body);

    let statuses: Vec<i32> = usage_rows(&s.db).await.into_iter().map(|r| r.1).collect();
    assert_eq!(statuses, vec![504, 200, 200, 502]);
    s.db.drop().await;
}

#[tokio::test]
async fn a_client_going_away_stops_the_upstream() {
    let Some(s) = setup().await else { return };
    let resp = api_raw(&s.db.app(), "/v1/chat/completions", &s.key, chat("local/endless", true)).await;
    let mut body = resp.into_body().into_data_stream();
    let first = body.next().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&first).contains("start"));
    drop(body);
    let mut waited = 0;
    while s.up.dropped() == 0 && waited < 50 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        waited += 1;
    }
    assert_eq!(s.up.dropped(), 1, "the upstream stream ended when the client left");
    let rows = usage_rows(&s.db).await;
    assert!(rows[0].5, "recorded as client_closed");
    s.db.drop().await;
}

#[tokio::test]
async fn credentials_are_sealed_and_a_wrong_secret_key_fails_loudly() {
    let Some(s) = setup().await else { return };
    let raw: Vec<u8> = sqlx::query_scalar("SELECT credential FROM connections WHERE id = $1")
        .bind(s.conn)
        .fetch_one(&s.db.pool)
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&raw).contains(UPSTREAM_KEY));
    let hdrs: Vec<u8> = sqlx::query_scalar("SELECT extra_headers FROM connections WHERE id = $1")
        .bind(s.conn)
        .fetch_one(&s.db.pool)
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&hdrs).contains("Converge test"));

    // The same database under another CONVERGE_SECRET_KEY.
    let mut other_key = SECRET;
    other_key[0] ^= 1;
    let other = converge_server::AppState {
        cookie_secure: false,
        ..converge_server::AppState::new(s.db.pool.clone(), &other_key)
    };
    other.reload().await.unwrap();
    let app = converge_server::router(other);
    let r = api(&app, "POST", "/v1/chat/completions", Some(&s.key), Some(chat("local/json", false))).await;
    assert_eq!(r.status, StatusCode::BAD_GATEWAY);
    assert!(r.body.contains("cannot be read"), "{}", r.body);
    assert!(s.up.seen().is_empty(), "never called without its credential");
    s.db.drop().await;
}

#[tokio::test]
async fn bad_requests_are_refused_before_the_upstream() {
    let Some(s) = setup().await else { return };
    let app = s.db.app();
    let r = api(&app, "POST", "/v1/chat/completions", Some(&s.key), Some(json!({"messages": []}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body.contains("model"));
    let r = api(&app, "POST", "/v1/chat/completions", Some(&s.key), Some(json!([1, 2]))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(s.up.seen().is_empty());
    s.db.drop().await;
}
