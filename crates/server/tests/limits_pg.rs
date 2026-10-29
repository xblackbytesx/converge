//! Limits and key scopes on `/v1`, against the stand-in upstream. Needs
//! `CONVERGE_TEST_DATABASE_URL`; skipped when unset.

mod common;

use std::time::Duration;

use axum::http::StatusCode;
use common::upstream::{UPSTREAM_KEY, Upstream};
use common::{Db, api, api_raw};
use converge_server::auth::Role;
use futures_util::StreamExt as _;
use serde_json::{Value, json};
use uuid::Uuid;

struct Setup {
    db: Db,
    up: Upstream,
    kim: Uuid,
    key: String,
    json: Uuid,
    endless: Uuid,
}

async fn setup() -> Option<Setup> {
    let db = Db::new().await?;
    let up = Upstream::start().await;
    let kim = db.person("kim@example.com", Role::User).await;
    let conn = db.connection("local", &up.base, Some(UPSTREAM_KEY)).await;
    let json = db.model(conn, "local/json", "json").await;
    let endless = db.model(conn, "local/endless", "endless").await;
    db.grant(kim, true, &[]).await;
    let key = db.key(kim).await;
    Some(Setup { db, up, kim, key, json, endless })
}

fn chat(model: &str) -> Value {
    json!({"model": model, "messages": [{"role": "user", "content": "hi"}]})
}

async fn limited(db: &Db) -> Vec<(i32, Option<String>, i64)> {
    db.flush().await;
    sqlx::query_as("SELECT status, limited, prompt_tokens + completion_tokens FROM usage ORDER BY id")
        .fetch_all(&db.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn requests_per_minute_and_tokens_per_day() {
    let Some(mut s) = setup().await else { return };
    s.db.limits(s.kim, Some(2), None, None).await;
    let app = s.db.app();
    for _ in 0..2 {
        assert_eq!(
            api(&app, "POST", "/v1/chat/completions", Some(&s.key), Some(chat("local/json"))).await.status,
            StatusCode::OK
        );
    }
    let before = s.up.seen().len();
    let r = api(&app, "POST", "/v1/chat/completions", Some(&s.key), Some(chat("local/json"))).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.headers["retry-after"], "30", "2 per minute: one every 30 s");
    let v: Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(v["error"]["code"], "rate_limit_exceeded");
    assert_eq!(v["error"]["type"], "rate_limit_error");
    assert!(v["error"]["message"].as_str().unwrap().starts_with("Requests per minute"), "{}", r.body);
    assert_eq!(s.up.seen().len(), before, "nothing reached the upstream");

    // Tokens are counted from the start, so a limit set mid-day starts
    // from what the day already used: 2 x 18.
    s.db.limits(s.kim, None, Some(30), None).await;
    let r = api(&app, "POST", "/v1/chat/completions", Some(&s.key), Some(chat("local/json"))).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(r.body.contains("Tokens per day: 36 used of 30, resets at 00:00 UTC"), "{}", r.body);
    let wait: u64 = r.headers["retry-after"].to_str().unwrap().parse().unwrap();
    assert!((1..=86_400).contains(&wait));

    // The request that crosses the line finishes; the next one waits.
    s.db.limits(s.kim, None, Some(50), None).await;
    assert_eq!(
        api(&app, "POST", "/v1/chat/completions", Some(&s.key), Some(chat("local/json"))).await.status,
        StatusCode::OK
    );
    let r = api(&app, "POST", "/v1/chat/completions", Some(&s.key), Some(chat("local/json"))).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(r.body.contains("54 used of 50"), "{}", r.body);

    assert_eq!(
        limited(&s.db).await,
        [
            (200, None, 18),
            (200, None, 18),
            (429, Some("rpm".into()), 0),
            (429, Some("tokens".into()), 0),
            (200, None, 18),
            (429, Some("tokens".into()), 0),
        ],
        "turned away by a limit: a row with status 429 and the limit"
    );

    // A restart does not hand out a fresh day.
    s.db.restart().await;
    let r = api(&s.db.app(), "POST", "/v1/chat/completions", Some(&s.key), Some(chat("local/json"))).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(r.body.contains("54 used of 50"), "seeded from today's rows: {}", r.body);
    s.db.drop().await;
}

#[tokio::test]
async fn parallel_requests_per_key() {
    let Some(s) = setup().await else { return };
    s.db.limits(s.kim, None, None, Some(1)).await;
    let other = s.db.key(s.kim).await;
    let app = s.db.app();
    let stream = json!({"model": "local/endless", "stream": true, "messages": [{"role": "user", "content": "hi"}]});
    let resp = api_raw(&app, "/v1/chat/completions", &s.key, stream).await;
    let mut body = resp.into_body().into_data_stream();
    assert!(String::from_utf8_lossy(&body.next().await.unwrap().unwrap()).contains("start"));

    let r = api(&app, "POST", "/v1/chat/completions", Some(&s.key), Some(chat("local/json"))).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.headers["retry-after"], "1");
    assert!(r.body.contains("Parallel requests for this key: 1"), "{}", r.body);
    let r = api(&app, "POST", "/v1/chat/completions", Some(&other), Some(chat("local/json"))).await;
    assert_eq!(r.status, StatusCode::OK, "per key: the person's other key still works");

    // The place comes back when the stream ends (here: the client leaves).
    drop(body);
    let mut ok = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let r = api(&app, "POST", "/v1/chat/completions", Some(&s.key), Some(chat("local/json"))).await;
        if r.status == StatusCode::OK {
            ok = true;
            break;
        }
    }
    assert!(ok, "the key's place was freed");
    let _ = s.endless;
    s.db.drop().await;
}

#[tokio::test]
async fn scoped_keys_reach_only_their_scope() {
    let Some(s) = setup().await else { return };
    let admin = s.db.person("admin@example.com", Role::Admin).await;
    let poet = s.db.persona(admin, "poet", s.endless, "Verse.", json!({})).await;
    converge_server::keys::set_grants(&s.db.pool, s.kim, false, &[s.json, s.endless], &[poet]).await.unwrap();
    s.db.reload().await;
    let key = s.db.scoped_key(s.kim, &[s.json], &[poet]).await;
    let app = s.db.app();

    let r = api(&app, "GET", "/v1/models", Some(&key), None).await;
    let v: Value = serde_json::from_str(&r.body).unwrap();
    let ids: Vec<&str> = v["data"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["local/json", "poet"]);
    let r = api(&app, "POST", "/v1/chat/completions", Some(&key), Some(chat("local/endless"))).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "out of scope looks like no such model");
    assert!(r.body.contains("model_not_found"));
    assert_eq!(api(&app, "GET", "/v1/models/local/endless", Some(&key), None).await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        api(&app, "POST", "/v1/chat/completions", Some(&key), Some(chat("local/json"))).await.status,
        StatusCode::OK
    );
    let full = api(&app, "GET", "/v1/models", Some(&s.key), None).await;
    assert!(full.body.contains("local/endless"), "an unscoped key reaches everything granted");

    // Taking the grant away takes it from the key.
    converge_server::keys::set_grants(&s.db.pool, s.kim, false, &[s.endless], &[poet]).await.unwrap();
    s.db.reload().await;
    let r = api(&app, "POST", "/v1/chat/completions", Some(&key), Some(chat("local/json"))).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    // Deleting everything in a scope leaves a key that reaches nothing.
    converge_server::personas::delete(&s.db.pool, poet).await.unwrap();
    converge_server::connections::delete_model(&s.db.pool, s.json).await.unwrap();
    s.db.reload().await;
    let r = api(&app, "GET", "/v1/models", Some(&key), None).await;
    assert_eq!(serde_json::from_str::<Value>(&r.body).unwrap()["data"], json!([]), "{}", r.body);
    assert!(api(&app, "GET", "/v1/models", Some(&s.key), None).await.body.contains("local/endless"));
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM key_scopes").fetch_one(&s.db.pool).await.unwrap();
    assert_eq!(n, 0, "scope rows go with what they named");

    let empty = converge_server::keys::KeyScope { models: vec![Uuid::new_v4()], personas: vec![] };
    let e = converge_server::keys::create(&s.db.pool, s.kim, "x", 0, Some(&empty)).await.unwrap_err();
    assert!(e.to_string().contains("at least one"), "{e}");
    s.db.drop().await;
}
