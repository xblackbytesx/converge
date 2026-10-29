//! Anthropic connections against the stand-in upstream: chat and Responses,
//! JSON and streams, personas, errors, import. Needs
//! `CONVERGE_TEST_DATABASE_URL`; skipped when unset.

mod common;

use axum::http::StatusCode;
use common::upstream::{UPSTREAM_KEY, Upstream};
use common::{Browser, Db, api};
use converge_core::anthropic::VERSION;
use converge_server::auth::Role;
use converge_server::connections::Kind;
use serde_json::{Value, json};
use uuid::Uuid;

struct Setup {
    db: Db,
    up: Upstream,
    key: String,
    admin: Uuid,
    conn: Uuid,
    m: std::collections::HashMap<&'static str, Uuid>,
}

async fn setup() -> Option<Setup> {
    let db = Db::new().await?;
    let up = Upstream::start().await;
    let admin = db.person("admin@example.com", Role::Admin).await;
    let kim = db.person("kim@example.com", Role::User).await;
    let conn = db.connection_kind(Kind::Anthropic, "claude", &up.base, Some(UPSTREAM_KEY), vec![], 5, 5).await;
    let mut m = std::collections::HashMap::new();
    for (id, up_name) in [
        ("claude/json", "claude-json"),
        ("claude/stream", "claude-stream"),
        ("claude/error", "claude-error"),
        ("claude/busy", "claude-busy"),
    ] {
        m.insert(id, db.model(conn, id, up_name).await);
    }
    db.grant(kim, true, &[]).await;
    let key = db.key(kim).await;
    Some(Setup { db, up, key, admin, conn, m })
}

/// The `data:` payloads of a chat stream.
fn chunks(body: &str) -> Vec<String> {
    body.split_terminator("\n\n").map(|e| e.strip_prefix("data: ").unwrap().to_string()).collect()
}

async fn usage(db: &Db) -> Vec<(String, String, i64, i64, i64)> {
    db.flush().await;
    sqlx::query_as(
        "SELECT model_name, endpoint, prompt_tokens, completion_tokens, cached_tokens FROM usage ORDER BY id",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn chat_reaches_claude_and_comes_back_as_chat() {
    let Some(s) = setup().await else { return };
    let app = s.db.app();
    let req = json!({
        "model": "claude/json",
        "temperature": 0.3,
        "messages": [{"role": "system", "content": "Be kind."}, {"role": "user", "content": "hi"}]
    });
    let r = api(&app, "POST", "/v1/chat/completions", Some(&s.key), Some(req)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.headers["converge-dropped-params"], "temperature");
    let v: Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(v["object"], "chat.completion");
    assert_eq!(v["model"], "claude/json", "what the client asked for");
    assert_eq!(v["choices"][0]["message"]["content"], "hi from claude");
    assert_eq!(v["choices"][0]["message"]["reasoning_content"], "Easy.");
    assert_eq!(v["usage"]["prompt_tokens"], 100);
    assert_eq!(v["usage"]["prompt_tokens_details"]["cached_tokens"], 90);

    let seen = s.up.last();
    assert_eq!(seen.path, "/v1/messages");
    assert_eq!(seen.headers["x-api-key"], UPSTREAM_KEY);
    assert_eq!(seen.headers["anthropic-version"], VERSION);
    assert!(seen.headers.get("authorization").is_none(), "Anthropic takes x-api-key only");
    assert_eq!(
        seen.body,
        json!({
            "model": "claude-json",
            "system": "Be kind.",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
            "max_tokens": 16000,
            "thinking": {"type": "adaptive", "display": "summarized"},
            "cache_control": {"type": "ephemeral"}
        })
    );

    // Streamed, without include_usage: chat chunks, no usage chunk, [DONE].
    let req = json!({"model": "claude/stream", "stream": true, "messages": [{"role": "user", "content": "hi"}]});
    let r = api(&app, "POST", "/v1/chat/completions", Some(&s.key), Some(req)).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers["content-type"], "text/event-stream");
    let c = chunks(&r.body);
    assert_eq!(c.last().unwrap(), "[DONE]");
    let parsed: Vec<Value> = c[..c.len() - 1].iter().map(|x| serde_json::from_str(x).unwrap()).collect();
    let text: String = parsed.iter().filter_map(|p| p["choices"][0]["delta"]["content"].as_str()).collect();
    assert_eq!(text, "Hello");
    let args: String = parsed
        .iter()
        .filter_map(|p| p["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"].as_str())
        .collect();
    assert_eq!(args, "{\"cmd\":\"ls\"}");
    assert!(parsed.iter().any(|p| p["choices"][0]["finish_reason"] == "tool_calls"));
    assert!(!parsed.iter().any(|p| p.get("usage").is_some()), "the client did not ask for usage");
    assert_eq!(s.up.last().body["max_tokens"], 64000);
    assert_eq!(s.up.last().body["stream"], true);

    let req = json!({"model": "claude/stream", "stream": true, "stream_options": {"include_usage": true}, "messages": [{"role": "user", "content": "hi"}]});
    let r = api(&app, "POST", "/v1/chat/completions", Some(&s.key), Some(req)).await;
    assert!(r.body.contains("\"prompt_tokens\":7"), "asked for, so passed on: {}", r.body);

    assert_eq!(
        usage(&s.db).await,
        [
            ("claude/json".to_string(), "chat/completions".to_string(), 100, 3, 90),
            ("claude/stream".to_string(), "chat/completions".to_string(), 7, 9, 0),
            ("claude/stream".to_string(), "chat/completions".to_string(), 7, 9, 0),
        ]
    );
    s.db.drop().await;
}

#[tokio::test]
async fn responses_reach_claude_through_both_translations() {
    let Some(s) = setup().await else { return };
    let app = s.db.app();
    let r = api(
        &app,
        "POST",
        "/v1/responses",
        Some(&s.key),
        Some(json!({"model": "claude/json", "instructions": "Be kind.", "input": "hi"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let v: Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(v["object"], "response");
    assert_eq!(v["output"][0]["type"], "reasoning");
    assert_eq!(v["output"][1]["content"][0]["text"], "hi from claude");
    assert_eq!(v["usage"]["input_tokens_details"]["cached_tokens"], 90);
    assert_eq!(s.up.last().body["system"], "Be kind.");

    let r = api(
        &app,
        "POST",
        "/v1/responses",
        Some(&s.key),
        Some(json!({"model": "claude/stream", "input": "hi", "stream": true})),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.starts_with("event: response.created"), "{}", r.body);
    assert!(!r.body.contains("[DONE]"));
    let last = r.body.split_terminator("\n\n").last().unwrap();
    let done: Value = serde_json::from_str(last.split_once("data: ").unwrap().1).unwrap();
    assert_eq!(done["type"], "response.completed");
    let out = &done["response"]["output"];
    assert_eq!(out[0]["content"][0]["text"], "Hello");
    assert_eq!(
        out[1],
        json!({"type": "function_call", "id": out[1]["id"], "call_id": "toolu_1", "name": "shell", "arguments": "{\"cmd\":\"ls\"}", "status": "completed"})
    );
    assert_eq!(done["response"]["usage"]["output_tokens"], 9);
    let u = usage(&s.db).await;
    assert_eq!(u[1], ("claude/stream".to_string(), "responses".to_string(), 7, 9, 0));
    s.db.drop().await;
}

#[tokio::test]
async fn personas_and_the_model_switches() {
    let Some(s) = setup().await else { return };
    let app = s.db.app();
    let params = json!({"max_tokens": {"mode": "lock", "value": 900}, "temperature": {"mode": "lock", "value": 0.1}});
    s.db.persona(s.admin, "poet", s.m["claude/json"], "Write verse.", params).await;
    converge_server::connections::set_model_switches(&s.db.pool, s.m["claude/json"], true, false, false).await.unwrap();
    s.db.reload().await;
    let req = json!({"model": "poet", "messages": [{"role": "system", "content": "Client."}, {"role": "user", "content": "hi"}]});
    let r = api(&app, "POST", "/v1/chat/completions", Some(&s.key), Some(req)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let v: Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(v["model"], "poet");
    assert_eq!(r.headers["converge-dropped-params"], "temperature", "the persona's lock cannot reach Claude");
    assert_eq!(
        s.up.last().body,
        json!({
            "model": "claude-json",
            "system": "Write verse.\n\nClient.",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
            "max_tokens": 900
        }),
        "reasoning and cache switched off: no thinking, no cache_control"
    );
    s.db.drop().await;
}

#[tokio::test]
async fn errors_endpoints_and_import() {
    let Some(s) = setup().await else { return };
    let app = s.db.app();
    let chat = |m: &str| json!({"model": m, "messages": [{"role": "user", "content": "hi"}]});
    let r = api(&app, "POST", "/v1/chat/completions", Some(&s.key), Some(chat("claude/error"))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let v: Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(
        v,
        json!({"error": {"message": "max_tokens: too large", "type": "invalid_request_error", "param": null, "code": "invalid_request_error"}})
    );
    let r = api(&app, "POST", "/v1/chat/completions", Some(&s.key), Some(chat("claude/busy"))).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE, "529 becomes a status OpenAI clients retry");
    let r =
        api(&app, "POST", "/v1/embeddings", Some(&s.key), Some(json!({"model": "claude/json", "input": "x"}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body.contains("Anthropic connection"), "{}", r.body);
    let bad = json!({"model": "claude/json", "n": 2, "messages": [{"role": "user", "content": "hi"}]});
    assert_eq!(
        api(&app, "POST", "/v1/chat/completions", Some(&s.key), Some(bad)).await.status,
        StatusCode::BAD_REQUEST
    );
    let statuses: Vec<i32> = {
        s.db.flush().await;
        sqlx::query_scalar("SELECT status FROM usage ORDER BY id").fetch_all(&s.db.pool).await.unwrap()
    };
    assert_eq!(statuses, [400, 529], "the upstream's own status is recorded; refused requests write nothing");

    let mut admin = Browser::logged_in(s.db.app(), "admin@example.com").await;
    let t = admin.htmx_post(&format!("/connections/{}/test", s.conn), &[]).await;
    assert!(t.body.contains("lists 3 models"), "both pages: {}", t.body);
    let page = admin.get(&format!("/connections/{}/import", s.conn)).await.body;
    assert!(page.contains("claude/claude-a") && page.contains("claude/claude-c"), "{page}");
    let list = admin.get("/connections").await.body;
    assert!(list.contains("badge\">Anthropic") && list.contains("name=\"cache\""), "{list}");
    s.db.drop().await;
}
