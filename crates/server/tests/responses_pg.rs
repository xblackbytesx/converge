//! `/v1/responses`, native and translated, against the stand-in upstream.
//! Needs `CONVERGE_TEST_DATABASE_URL`; skipped when unset.

mod common;

use axum::http::StatusCode;
use common::upstream::{RESP_JSON, RESP_STREAM, UPSTREAM_KEY, Upstream};
use common::{Db, api};
use converge_server::auth::Role;
use serde_json::{Value, json};
use uuid::Uuid;

struct Setup {
    db: Db,
    up: Upstream,
    key: String,
    admin: Uuid,
    /// Model ids by public id.
    m: std::collections::HashMap<&'static str, Uuid>,
}

/// `local` translates; `oai` is declared native. Same stand-in behind both.
async fn setup() -> Option<Setup> {
    let db = Db::new().await?;
    let up = Upstream::start().await;
    let admin = db.person("admin@example.com", Role::Admin).await;
    let kim = db.person("kim@example.com", Role::User).await;
    let local = db.connection("local", &up.base, Some(UPSTREAM_KEY)).await;
    let oai = db.connection("oai", &up.base, Some(UPSTREAM_KEY)).await;
    sqlx::query("UPDATE connections SET responses = 'native' WHERE id = $1").bind(oai).execute(&db.pool).await.unwrap();
    let mut m = std::collections::HashMap::new();
    for (conn, id, up_name) in [
        (local, "local/json", "json"),
        (local, "local/stream", "stream"),
        (local, "local/tools", "tools-stream"),
        (local, "local/think", "think-stream"),
        (local, "local/think-json", "think-json"),
        (local, "local/error", "error"),
        (oai, "oai/resp-json", "resp-json"),
        (oai, "oai/resp-stream", "resp-stream"),
    ] {
        m.insert(id, db.model(conn, id, up_name).await);
    }
    db.grant(kim, true, &[]).await;
    let key = db.key(kim).await;
    Some(Setup { db, up, key, admin, m })
}

/// The events of a Responses stream, as (event line, data).
fn events(body: &str) -> Vec<(String, Value)> {
    body.split_terminator("\n\n")
        .map(|e| {
            let (ev, data) = e.split_once('\n').unwrap();
            let v: Value = serde_json::from_str(data.strip_prefix("data: ").unwrap()).unwrap();
            (ev.strip_prefix("event: ").unwrap().to_string(), v)
        })
        .collect()
}

async fn usage(db: &Db) -> Vec<(String, String, i64, i64)> {
    db.flush().await;
    sqlx::query_as("SELECT model_name, endpoint, prompt_tokens, completion_tokens FROM usage ORDER BY id")
        .fetch_all(&db.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn translated_answers_come_back_as_responses() {
    let Some(s) = setup().await else { return };
    let app = s.db.app();
    let req = json!({"model": "local/json", "input": "hi", "store": false, "include": ["reasoning.encrypted_content"]});
    let r = api(&app, "POST", "/v1/responses", Some(&s.key), Some(req)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.headers["converge-dropped-params"], "include, store");
    let v: Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(v["object"], "response");
    assert!(v["id"].as_str().unwrap().starts_with("resp_") && v["id"].as_str().unwrap().len() == 37);
    assert_eq!(v["model"], "local/json");
    assert_eq!(v["output"][0]["content"][0]["text"], "hi");
    assert_eq!(v["usage"]["input_tokens"], 11);
    assert_eq!(v["usage"]["input_tokens_details"]["cached_tokens"], 2);
    let seen = s.up.last();
    assert_eq!(seen.path, "/v1/chat/completions");
    assert_eq!(seen.body, json!({"model": "json", "messages": [{"role": "user", "content": "hi"}]}));

    // Streamed: Responses events, usage asked for upstream and read back.
    let req = json!({"model": "local/stream", "input": "hi", "stream": true});
    let r = api(&app, "POST", "/v1/responses", Some(&s.key), Some(req)).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers["content-type"], "text/event-stream");
    assert!(r.headers.get("converge-dropped-params").is_none());
    assert_eq!(s.up.last().body["stream_options"], json!({"include_usage": true}));
    let e = events(&r.body);
    assert_eq!(e.first().unwrap().0, "response.created");
    assert_eq!(e.last().unwrap().0, "response.completed");
    assert!(!r.body.contains("[DONE]"));
    let done = &e.last().unwrap().1["response"];
    assert_eq!(done["output"][0]["content"][0]["text"], "Hello");
    assert_eq!(done["usage"]["output_tokens"], 3);

    // Tool calls, and reasoning per model.
    let r = api(
        &app,
        "POST",
        "/v1/responses",
        Some(&s.key),
        Some(json!({"model": "local/tools", "input": "x", "stream": true})),
    )
    .await;
    let out = events(&r.body).last().unwrap().1["response"]["output"].clone();
    assert_eq!(out[0]["call_id"], "call_a");
    assert_eq!(out[0]["arguments"], "{\"cmd\":\"ls\"}");
    assert_eq!(out[1]["name"], "read");
    let r = api(
        &app,
        "POST",
        "/v1/responses",
        Some(&s.key),
        Some(json!({"model": "local/think", "input": "x", "stream": true})),
    )
    .await;
    assert!(r.body.contains("response.reasoning_text.delta"));
    let r = api(&app, "POST", "/v1/responses", Some(&s.key), Some(json!({"model": "local/think-json", "input": "x"})))
        .await;
    let v: Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(v["output"][0]["type"], "reasoning");
    converge_server::connections::set_model_switches(&s.db.pool, s.m["local/think"], true, false, true).await.unwrap();
    converge_server::connections::set_model_switches(&s.db.pool, s.m["local/think-json"], true, false, true)
        .await
        .unwrap();
    s.db.reload().await;
    let r = api(
        &app,
        "POST",
        "/v1/responses",
        Some(&s.key),
        Some(json!({"model": "local/think", "input": "x", "stream": true})),
    )
    .await;
    assert!(!r.body.contains("reasoning_text") && !r.body.contains("\"type\":\"reasoning\""), "{}", r.body);
    assert!(r.body.contains("response.completed"));
    let r = api(&app, "POST", "/v1/responses", Some(&s.key), Some(json!({"model": "local/think-json", "input": "x"})))
        .await;
    let v: Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(v["output"][0]["type"], "message", "reasoning switched off for this model");

    // Upstream errors pass through as they are.
    let r = api(&app, "POST", "/v1/responses", Some(&s.key), Some(json!({"model": "local/error", "input": "x"}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body.contains("bad things"));

    let u = usage(&s.db).await;
    assert_eq!(u[0], ("local/json".to_string(), "responses".to_string(), 11, 7));
    assert_eq!(u[1], ("local/stream".to_string(), "responses".to_string(), 5, 3));
    s.db.drop().await;
}

#[tokio::test]
async fn native_connections_pass_responses_through() {
    let Some(s) = setup().await else { return };
    let app = s.db.app();
    let req = json!({"model": "oai/resp-json", "input": "hi", "previous_response_id": "resp_up0", "store": true});
    let r = api(&app, "POST", "/v1/responses", Some(&s.key), Some(req)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body, RESP_JSON, "byte for byte");
    let seen = s.up.last();
    assert_eq!(seen.path, "/v1/responses");
    assert_eq!(
        seen.body,
        json!({"model": "resp-json", "input": "hi", "previous_response_id": "resp_up0", "store": true})
    );

    let r = api(
        &app,
        "POST",
        "/v1/responses",
        Some(&s.key),
        Some(json!({"model": "oai/resp-stream", "input": "hi", "stream": true})),
    )
    .await;
    assert_eq!(r.body, RESP_STREAM);
    assert!(s.up.last().body.get("stream_options").is_none(), "Responses streams carry usage anyway");
    assert_eq!(
        usage(&s.db).await,
        [
            ("oai/resp-json".to_string(), "responses".to_string(), 8, 4),
            ("oai/resp-stream".to_string(), "responses".to_string(), 6, 2)
        ]
    );
    s.db.drop().await;
}

#[tokio::test]
async fn personas_work_on_responses_both_ways() {
    let Some(s) = setup().await else { return };
    let app = s.db.app();
    let params = json!({"temperature": {"mode": "lock", "value": 0.2}, "seed": {"mode": "lock", "value": 7}});
    let id = s.db.persona(s.admin, "designer", s.m["local/json"], "Design.", params).await;
    let req = json!({"model": "designer", "instructions": "Client.", "input": "hi", "temperature": 1});

    let r = api(&app, "POST", "/v1/responses", Some(&s.key), Some(req.clone())).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let v: Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(v["model"], "designer", "never the model behind it");
    let seen = s.up.last().body;
    assert_eq!(seen["messages"][0], json!({"role": "system", "content": "Design.\n\nClient."}));
    assert_eq!((seen["temperature"].clone(), seen["seed"].clone()), (json!(0.2), json!(7)));

    // Swapped onto the native connection: the prompt goes into instructions.
    sqlx::query("UPDATE personas SET model_id = $2 WHERE id = $1")
        .bind(id)
        .bind(s.m["oai/resp-json"])
        .execute(&s.db.pool)
        .await
        .unwrap();
    s.db.reload().await;
    let r = api(&app, "POST", "/v1/responses", Some(&s.key), Some(req)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let seen = s.up.last();
    assert_eq!(seen.path, "/v1/responses");
    assert_eq!(
        seen.body,
        json!({"model": "resp-json", "instructions": "Design.\n\nClient.", "input": "hi", "temperature": 0.2}),
        "seed has no Responses field"
    );
    let r = api(&app, "POST", "/v1/embeddings", Some(&s.key), Some(json!({"model": "designer", "input": "x"}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    s.db.drop().await;
}

#[tokio::test]
async fn what_cannot_be_translated_or_routed_is_refused() {
    let Some(s) = setup().await else { return };
    let app = s.db.app();
    let before = s.up.seen().len();
    for (req, says) in [
        (json!({"model": "local/json", "input": "x", "previous_response_id": "resp_1"}), "store: false"),
        (json!({"model": "local/json", "input": "x", "tools": [{"type": "web_search"}]}), "`web_search` tool"),
        (json!({"model": "local/json"}), "`input` is required"),
    ] {
        let r = api(&app, "POST", "/v1/responses", Some(&s.key), Some(req)).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        assert!(r.body.contains(says) && r.body.contains("invalid_request_error"), "{}", r.body);
    }
    assert_eq!(s.up.seen().len(), before, "nothing reached the upstream");
    assert!(usage(&s.db).await.is_empty(), "refused before the upstream, like unknown models");

    for (method, path) in [
        ("GET", "/v1/responses/resp_1"),
        ("DELETE", "/v1/responses/resp_1"),
        ("POST", "/v1/responses/resp_1/cancel"),
        ("GET", "/v1/responses/resp_1/input_items"),
    ] {
        let r = api(&app, method, path, Some(&s.key), None).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{method} {path}");
        assert!(r.body.contains("stored responses"));
        assert_eq!(api(&app, method, path, None, None).await.status, StatusCode::UNAUTHORIZED);
    }
    s.db.drop().await;
}
