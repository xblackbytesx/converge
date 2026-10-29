//! The Playground (M8): `/playground/send` against the stand-in upstream,
//! drafts, saving, and what users may see. Needs
//! `CONVERGE_TEST_DATABASE_URL`; skipped when unset.

mod common;

use axum::http::StatusCode;
use common::upstream::{UPSTREAM_KEY, Upstream};
use common::{Browser, Db};
use converge_server::auth::Role;
use serde_json::{Value, json};
use uuid::Uuid;

struct Setup {
    db: Db,
    up: Upstream,
    kim: Uuid,
    stream: Uuid,
    other: Uuid,
    persona: Uuid,
}

async fn setup() -> Option<Setup> {
    let db = Db::new().await?;
    let up = Upstream::start().await;
    let admin = db.person("admin@example.com", Role::Admin).await;
    let kim = db.person("kim@example.com", Role::User).await;
    let conn = db.connection("local", &up.base, Some(UPSTREAM_KEY)).await;
    let stream = db.model(conn, "local/stream", "stream").await;
    let other = db.model(conn, "local/inline", "inline-usage").await;
    let params = json!({"temperature": {"mode": "lock", "value": 0.2}});
    let persona = db.persona(admin, "poet", stream, "Write verse.", params).await;
    Some(Setup { db, up, kim, stream, other, persona })
}

fn turn(target: &str) -> Value {
    json!({"target": target, "messages": [{"role": "user", "content": "hi"}]})
}

/// The `data:` payload of each event, with its `event:` name.
fn events(body: &str) -> Vec<(String, String)> {
    body.split_terminator("\n\n")
        .map(|e| {
            let mut name = "message".to_string();
            let mut data = String::new();
            for line in e.lines() {
                if let Some(n) = line.strip_prefix("event: ") {
                    name = n.to_string();
                } else if let Some(d) = line.strip_prefix("data: ") {
                    data = d.to_string();
                }
            }
            (name, data)
        })
        .collect()
}

/// The page's data attribute, unescaped.
fn page_data(html: &str) -> Value {
    let raw = html.split("data-playground=\"").nth(1).unwrap().split('"').next().unwrap();
    // askama writes numeric entities; a browser decodes them in getAttribute.
    let raw = raw.replace("&#34;", "\"").replace("&#39;", "'").replace("&#60;", "<").replace("&#62;", ">");
    serde_json::from_str(&raw.replace("&#38;", "&")).unwrap()
}

#[tokio::test]
async fn admins_stream_tune_drafts_and_save_them() {
    let Some(s) = setup().await else { return };
    let mut admin = Browser::logged_in(s.db.app(), "admin@example.com").await;

    let data = page_data(&admin.get("/playground?pick=poet").await.body);
    assert_eq!(data["pick"], "poet");
    assert_eq!(data["personas"][0]["model"], "local/stream", "admins see the model behind a persona");

    // A persona, streamed, with the column's own settings and prompt.
    let mut req = turn("poet");
    req["system"] = json!("Client.");
    req["params"] = json!({"temperature": "1.5", "max_tokens": "50"});
    let r = admin.post_json("/playground/send", &req).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.headers["content-type"], "text/event-stream");
    let e = events(&r.body);
    assert_eq!(e[0].0, "converge", "what the request became comes first");
    let info: Value = serde_json::from_str(&e[0].1).unwrap();
    assert_eq!((info["persona"].as_str(), info["model"].as_str()), (Some("poet"), Some("local/stream")));
    assert_eq!(info["upstream_model"], "stream");
    assert_eq!(info["sent"]["messages"][0]["content"], "Write verse.\n\nClient.");
    assert_eq!(info["sent"]["temperature"], 0.2, "the persona's lock beats the column's value");
    assert_eq!(info["sent"]["max_tokens"], 50);
    assert!(r.body.contains("Hel") && r.body.contains("[DONE]"));
    assert!(r.body.contains("\"prompt_tokens\":5"), "usage is asked for and passed on: {}", r.body);
    assert_eq!(s.up.last().body, info["sent"], "the shown body is the one that went upstream");

    // A draft: another model, prompt and lock, without touching the persona.
    let seed: Value =
        serde_json::from_str(&admin.get(&format!("/playground/persona/{}", s.persona)).await.body).unwrap();
    assert_eq!(seed["prompt"], "Write verse.");
    assert_eq!(seed["params"]["temperature_mode"], "lock");
    let draft = json!({
        "persona": s.persona, "model": s.other, "prompt": "Write haiku.", "mode": "replace",
        "params": {"temperature_mode": "lock", "temperature_value": "0.9"}
    });
    let mut req = turn("poet");
    req["system"] = json!("Client.");
    req["draft"] = draft.clone();
    let r = admin.post_json("/playground/send", &req).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let seen = s.up.last().body;
    assert_eq!(seen["model"], "inline-usage");
    assert_eq!(seen["messages"][0], json!({"role": "system", "content": "Write haiku."}), "replace mode");
    assert_eq!(seen["temperature"], 0.9);
    let saved = converge_server::personas::get(&s.db.pool, s.persona).await.unwrap().unwrap();
    assert_eq!((saved.system_prompt.as_str(), saved.model_id), ("Write verse.", s.stream), "the draft is not saved");

    // Saving it goes through the normal update, with history.
    let r = admin.post_json(&format!("/playground/persona/{}", s.persona), &draft).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let saved = converge_server::personas::get(&s.db.pool, s.persona).await.unwrap().unwrap();
    assert_eq!(
        (saved.system_prompt.as_str(), saved.model_id, saved.prompt_mode.as_str()),
        ("Write haiku.", s.other, "replace")
    );
    let history: i64 = sqlx::query_scalar("SELECT count(*) FROM persona_history WHERE persona_id = $1")
        .bind(s.persona)
        .fetch_one(&s.db.pool)
        .await
        .unwrap();
    assert!(history >= 2, "created, then the save");
    let bad = json!({"persona": s.persona, "model": s.other, "prompt": "", "mode": "prepend",
        "params": {"temperature_mode": "lock", "temperature_value": "9"}});
    let r = admin.post_json(&format!("/playground/persona/{}", s.persona), &bad).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body.contains("temperature must be between"), "{}", r.body);

    // Errors come back as JSON with the same information.
    let r = admin.post_json("/playground/send", &json!({"target": "poet", "messages": []})).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = admin.post_json("/playground/send", &turn("nope")).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    s.db.flush().await;
    let rows: Vec<(Option<Uuid>, String)> =
        sqlx::query_as("SELECT key_id, model_name FROM usage ORDER BY id").fetch_all(&s.db.pool).await.unwrap();
    assert_eq!(rows, [(None, "local/stream".into()), (None, "local/inline".into())], "usage without a key");
    let usage = admin.get("/usage").await.body;
    assert!(usage.contains("Website (Playground)"), "{usage}");
    s.db.drop().await;
}

#[tokio::test]
async fn users_get_their_own_models_and_never_the_admin_parts() {
    let Some(s) = setup().await else { return };
    converge_server::keys::set_grants(&s.db.pool, s.kim, false, &[s.other], &[s.persona]).await.unwrap();
    s.db.reload().await;
    let mut kim = Browser::logged_in(s.db.app(), "kim@example.com").await;

    let data = page_data(&kim.get("/playground").await.body);
    assert_eq!(data["admin"], false);
    let ids: Vec<&str> = data["models"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["local/inline"]);
    assert_eq!(data["personas"][0]["model"], Value::Null, "users never see the model behind a persona");

    let r = kim.post_json("/playground/send", &turn("poet")).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let info: Value = serde_json::from_str(&events(&r.body)[0].1).unwrap();
    assert_eq!(info, json!({"model": null, "persona": "poet", "dropped": []}), "no body, no model");
    assert_eq!(kim.post_json("/playground/send", &turn("local/stream")).await.status, StatusCode::NOT_FOUND);

    let mut req = turn("poet");
    req["draft"] = json!({"persona": s.persona, "model": s.other, "prompt": "x", "mode": "prepend"});
    assert_eq!(kim.post_json("/playground/send", &req).await.status, StatusCode::FORBIDDEN);
    assert_eq!(kim.get(&format!("/playground/persona/{}", s.persona)).await.status, StatusCode::FORBIDDEN);
    let r = kim.post_json(&format!("/playground/persona/{}", s.persona), &req["draft"]).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    // Limits apply, as for any client of hers.
    s.db.limits(s.kim, Some(1), None, None).await;
    let r = kim.post_json("/playground/send", &turn("local/inline")).await;
    assert_eq!(r.status, StatusCode::OK);
    let r = kim.post_json("/playground/send", &turn("local/inline")).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS, "{}", r.body);
    let v: Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(v["answer"]["error"]["code"], "rate_limit_exceeded");

    // Cross-site and logged out.
    let cross = kim
        .send(
            axum::http::Request::post("/playground/send").header("content-type", "application/json"),
            axum::body::Body::from(turn("poet").to_string()),
            Some("https://evil.example"),
        )
        .await;
    assert_eq!(cross.status, StatusCode::FORBIDDEN);
    let mut anon = Browser::new(s.db.app());
    assert_eq!(anon.post_json("/playground/send", &turn("poet")).await.status, StatusCode::SEE_OTHER);
    s.db.drop().await;
}
