//! Personas end to end: `/v1` against the stand-in upstream and the persona
//! pages. Needs `CONVERGE_TEST_DATABASE_URL`; skipped when unset.

mod common;

use axum::http::StatusCode;
use common::upstream::{CHAT_JSON, UPSTREAM_KEY, Upstream};
use common::{Browser, Db, api};
use converge_core::persona::Params;
use converge_server::auth::Role;
use serde_json::{Value, json};
use uuid::Uuid;

struct Setup {
    db: Db,
    up: Upstream,
    admin: Uuid,
    kim: Uuid,
    /// `local/json` and `cloud/json`: the same stand-in behind two
    /// connections, so a swap shows in usage.
    local: Uuid,
    cloud: Uuid,
    cloud_conn: Uuid,
}

async fn setup() -> Option<Setup> {
    let db = Db::new().await?;
    let up = Upstream::start().await;
    let admin = db.person("admin@example.com", Role::Admin).await;
    let kim = db.person("kim@example.com", Role::User).await;
    let local_conn = db.connection("local", &up.base, Some(UPSTREAM_KEY)).await;
    let cloud_conn = db.connection("cloud", &up.base, Some(UPSTREAM_KEY)).await;
    let local = db.model(local_conn, "local/json", "json").await;
    db.model(local_conn, "local/embed", "embed").await;
    let cloud = db.model(cloud_conn, "cloud/json", "json").await;
    Some(Setup { db, up, admin, kim, local, cloud, cloud_conn })
}

fn chat(model: &str) -> Value {
    json!({"model": model, "messages": [{"role": "user", "content": "hi"}]})
}

fn ids(body: &str) -> Vec<String> {
    let v: Value = serde_json::from_str(body).unwrap();
    v["data"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap().to_string()).collect()
}

/// The persona form with every field, as the browser sends it.
fn form(model: Uuid, prompt: &str, extra: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut f: Vec<(String, String)> = [
        ("title", "Designer"),
        ("public_id", "designer"),
        ("description", "Draws things"),
        ("system_prompt", prompt),
        ("prompt_mode", "prepend"),
        ("prompt_visible", "on"),
        ("enabled", "on"),
        ("temperature_mode", "lock"),
        ("temperature_value", "0.2"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    f.push(("model_id".into(), model.to_string()));
    for (k, v) in extra {
        f.retain(|(n, _)| n != k);
        f.push((k.to_string(), v.to_string()));
    }
    f
}

async fn post(b: &mut Browser, path: &str, f: &[(String, String)]) -> common::Resp {
    let pairs: Vec<(&str, &str)> = f.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    b.post(path, &pairs).await
}

async fn usage(db: &Db) -> Vec<(String, Option<String>, bool)> {
    db.flush().await;
    sqlx::query_as("SELECT model_name, persona_name, key_id IS NOT NULL FROM usage ORDER BY id")
        .fetch_all(&db.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_persona_grant_is_enough_and_the_prompt_and_settings_arrive() {
    let Some(s) = setup().await else { return };
    let app = s.db.app();
    let designer =
        s.db.persona(
            s.admin,
            "designer",
            s.local,
            "Design well.",
            json!({"temperature": {"mode": "lock", "value": 0.2}, "max_tokens": {"mode": "default", "value": 300}}),
        )
        .await;
    let model_defaults = Params::from_json(&json!({"top_p": {"mode": "default", "value": 0.5}}));
    converge_server::connections::set_model_params(&s.db.pool, s.local, &model_defaults).await.unwrap();
    s.db.grant_personas(s.kim, &[designer]).await;
    let key = s.db.key(s.kim).await;

    let r = api(&app, "GET", "/v1/models", Some(&key), None).await;
    assert_eq!(ids(&r.body), ["designer"]);
    let v: Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(v["data"][0]["owned_by"], "persona");
    assert_eq!(v["data"][0]["converge"], json!({"kind": "persona", "title": "The designer", "description": "Helps."}));
    assert!(!r.body.contains("local/json"), "never the model behind it");
    assert_eq!(api(&app, "GET", "/v1/models/designer", Some(&key), None).await.status, StatusCode::OK);
    assert_eq!(api(&app, "GET", "/v1/models/local/json", Some(&key), None).await.status, StatusCode::NOT_FOUND);

    let body = json!({
        "model": "designer",
        "temperature": 1.5,
        "messages": [{"role": "system", "content": "From the client."}, {"role": "user", "content": "hi"}]
    });
    let r = api(&app, "POST", "/v1/chat/completions", Some(&key), Some(body)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body, CHAT_JSON);
    let seen = s.up.last().body;
    assert_eq!(seen["model"], "json");
    assert_eq!(seen["temperature"], 0.2, "the lock wins over the client");
    assert_eq!(seen["max_tokens"], 300, "persona default");
    assert_eq!(seen["top_p"], 0.5, "model default under the persona");
    assert_eq!(
        seen["messages"],
        json!([{"role": "system", "content": "Design well.\n\nFrom the client."}, {"role": "user", "content": "hi"}])
    );

    let r = api(&app, "POST", "/v1/chat/completions", Some(&key), Some(chat("local/json"))).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "no grant on the model itself");
    let r = api(&app, "POST", "/v1/embeddings", Some(&key), Some(json!({"model": "designer", "input": "x"}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body.contains("personas work with /v1/chat/completions"), "{}", r.body);

    // Model defaults also apply to the model used directly.
    let admin_key = s.db.key(s.admin).await;
    let r = api(&app, "POST", "/v1/chat/completions", Some(&admin_key), Some(chat("local/json"))).await;
    assert_eq!(r.status, StatusCode::OK);
    let seen = s.up.last().body;
    assert_eq!(seen["top_p"], 0.5);
    assert!(seen.get("temperature").is_none() && seen["messages"].as_array().unwrap().len() == 1);

    assert_eq!(
        usage(&s.db).await,
        [("local/json".to_string(), Some("designer".to_string()), true), ("local/json".to_string(), None, true)]
    );
    s.db.drop().await;
}

#[tokio::test]
async fn swaps_take_effect_on_the_next_request_and_can_be_undone() {
    let Some(s) = setup().await else { return };
    let app = s.db.app();
    let key = s.db.key(s.admin).await;
    let mut admin = Browser::logged_in(s.db.app(), "admin@example.com").await;

    let r = post(&mut admin, "/personas", &form(s.local, "Design.\r\n", &[])).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let page = r.headers["location"].to_str().unwrap().to_string();
    let id: Uuid = page.trim_start_matches("/personas/").parse().unwrap();
    assert_eq!(
        api(&app, "POST", "/v1/chat/completions", Some(&key), Some(chat("designer"))).await.status,
        StatusCode::OK
    );
    assert_eq!(s.up.last().body["messages"][0]["content"], "Design.");

    // Swap: the very next request goes to the other model.
    let r = post(&mut admin, &page, &form(s.cloud, "Design.", &[])).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(
        api(&app, "POST", "/v1/chat/completions", Some(&key), Some(chat("designer"))).await.status,
        StatusCode::OK
    );

    // A prompt edit and a settings edit, each undoable.
    let r = post(&mut admin, &page, &form(s.cloud, "Design boldly.", &[("temperature_mode", "off")])).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    api(&app, "POST", "/v1/chat/completions", Some(&key), Some(chat("designer"))).await;
    let seen = s.up.last().body;
    assert_eq!(seen["messages"][0]["content"], "Design boldly.");
    assert!(seen.get("temperature").is_none());

    let history: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, what FROM persona_history WHERE persona_id = $1 ORDER BY id")
            .bind(id)
            .fetch_all(&s.db.pool)
            .await
            .unwrap();
    let whats: Vec<&str> = history.iter().map(|(_, w)| w.as_str()).collect();
    assert_eq!(whats, ["created", "model", "prompt", "settings"]);
    let body = admin.get(&page).await.body;
    assert!(body.contains("Model local/json to cloud/json") && body.contains("Switch back"), "{body}");
    assert!(body.contains("temperature 0.2 (lock)"), "settings before the change are shown");

    for (h, _) in &history[1..] {
        let r = admin.post(&format!("{page}/history/{h}/restore"), &[]).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    }
    api(&app, "POST", "/v1/chat/completions", Some(&key), Some(chat("designer"))).await;
    let seen = s.up.last().body;
    assert_eq!(seen["messages"][0]["content"], "Design.");
    assert_eq!(seen["temperature"], 0.2);
    let models: Vec<String> = usage(&s.db).await.into_iter().map(|u| u.0).collect();
    assert_eq!(models, ["local/json", "cloud/json", "cloud/json", "local/json"]);
    let undos: i64 = sqlx::query_scalar("SELECT count(*) FROM persona_history WHERE persona_id = $1")
        .bind(id)
        .fetch_one(&s.db.pool)
        .await
        .unwrap();
    assert_eq!(undos, 7, "each undo is a change of its own");
    s.db.drop().await;
}

#[tokio::test]
async fn models_in_use_cannot_be_removed_and_ids_are_one_namespace() {
    let Some(s) = setup().await else { return };
    let designer = s.db.persona(s.admin, "designer", s.cloud, "", json!({})).await;
    let mut admin = Browser::logged_in(s.db.app(), "admin@example.com").await;

    let r = admin.htmx_post(&format!("/models/{}/delete", s.cloud), &[]).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body.contains("designer") && r.body.contains("move them"), "{}", r.body);
    let r = admin.htmx_post(&format!("/connections/{}/delete", s.cloud_conn), &[]).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.body);
    assert!(
        sqlx::query("DELETE FROM models WHERE id = $1").bind(s.cloud).execute(&s.db.pool).await.is_err(),
        "the database refuses too"
    );
    let page = admin.get("/connections").await.body;
    assert!(page.contains("Personas on it") && page.contains(&format!("/personas/{designer}")));

    // One namespace: a persona cannot take a model's id, nor the reverse.
    let r = admin.htmx_post("/personas", &pairs(&form(s.local, "x", &[("public_id", "local/json")]))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body.contains("already in use"), "{}", r.body);
    let r = admin.htmx_post(&format!("/models/{}", s.local), &[("public_id", "designer"), ("enabled", "on")]).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);

    assert_eq!(admin.post(&format!("/personas/{designer}/delete"), &[]).await.status, StatusCode::SEE_OTHER);
    assert_eq!(admin.post(&format!("/models/{}/delete", s.cloud), &[]).await.status, StatusCode::SEE_OTHER);
    s.db.drop().await;
}

fn pairs(f: &[(String, String)]) -> Vec<(&str, &str)> {
    f.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
}

#[tokio::test]
async fn paused_and_disabled_personas_are_not_found() {
    let Some(s) = setup().await else { return };
    let app = s.db.app();
    let id = s.db.persona(s.admin, "designer", s.local, "", json!({})).await;
    let key = s.db.key(s.admin).await;
    let mut admin = Browser::logged_in(s.db.app(), "admin@example.com").await;

    assert!(ids(&api(&app, "GET", "/v1/models", Some(&key), None).await.body).contains(&"designer".to_string()));
    assert_eq!(
        admin.post(&format!("/models/{}", s.local), &[("public_id", "local/json")]).await.status,
        StatusCode::SEE_OTHER
    );
    let r = api(&app, "POST", "/v1/chat/completions", Some(&key), Some(chat("designer"))).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert!(r.body.contains("model_not_found"));
    assert!(!ids(&api(&app, "GET", "/v1/models", Some(&key), None).await.body).contains(&"designer".to_string()));
    assert!(admin.get(&format!("/personas/{id}")).await.body.contains("Paused"));
    assert!(admin.get("/personas").await.body.contains("badge warn\">paused"));
    let t = admin
        .post_json(
            "/playground/send",
            &json!({"target": "designer", "messages": [{"role": "user", "content": "hello"}]}),
        )
        .await;
    assert_eq!(t.status, StatusCode::NOT_FOUND, "paused: like a client, the Playground cannot reach it");

    assert_eq!(
        admin.post(&format!("/models/{}", s.local), &[("public_id", "local/json"), ("enabled", "on")]).await.status,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        api(&app, "POST", "/v1/chat/completions", Some(&key), Some(chat("designer"))).await.status,
        StatusCode::OK
    );
    let off = form(s.local, "", &[]).into_iter().filter(|(k, _)| k != "enabled").collect::<Vec<_>>();
    assert_eq!(post(&mut admin, &format!("/personas/{id}"), &off).await.status, StatusCode::SEE_OTHER);
    assert_eq!(
        api(&app, "POST", "/v1/chat/completions", Some(&key), Some(chat("designer"))).await.status,
        StatusCode::NOT_FOUND
    );
    s.db.drop().await;
}

#[tokio::test]
async fn users_see_granted_personas_the_prompt_can_be_private_and_the_playground_works() {
    let Some(s) = setup().await else { return };
    let id = s.db.persona(s.admin, "designer", s.local, "Secret sauce.", json!({})).await;
    let mut admin = Browser::logged_in(s.db.app(), "admin@example.com").await;
    let mut kim = Browser::logged_in(s.db.app(), "kim@example.com").await;
    let page = format!("/personas/{id}");

    assert_eq!(kim.get(&page).await.status, StatusCode::NOT_FOUND, "no grant, no page");
    assert!(!kim.get("/personas").await.body.contains("designer"));
    let r = admin.post(&format!("/admin/people/{}/access", s.kim), &[("persona", &id.to_string())]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(admin.get(&format!("/admin/people/{}/access", s.kim)).await.body.contains("Or these personas"));
    let list = kim.get("/personas").await.body;
    assert!(list.contains("The designer") && !list.contains("local/json"), "users never see the model");
    let view = kim.get(&page).await.body;
    assert!(view.contains("Secret sauce.") && view.contains("designer"));
    assert!(!view.contains("local/json"));

    let private =
        form(s.local, "Secret sauce.", &[]).into_iter().filter(|(k, _)| k != "prompt_visible").collect::<Vec<_>>();
    assert_eq!(post(&mut admin, &page, &private).await.status, StatusCode::SEE_OTHER);
    let view = kim.get(&page).await.body;
    assert!(!view.contains("Secret sauce.") && view.contains("keeps this prompt private"));
    assert_eq!(kim.post(&page, &pairs(&private)).await.status, StatusCode::FORBIDDEN, "users cannot edit");

    let t = admin
        .post_json(
            "/playground/send",
            &json!({"target": "designer", "messages": [{"role": "user", "content": "hello"}]}),
        )
        .await;
    assert_eq!(t.status, StatusCode::OK);
    let v: Value = serde_json::from_str(&t.body).unwrap();
    assert_eq!(v["answer"]["choices"][0]["message"]["content"], "hi", "answered whole: {}", t.body);
    assert_eq!(v["converge"]["model"], "local/json");
    assert_eq!(s.up.last().body["messages"][0]["content"], "Secret sauce.");
    assert_eq!(usage(&s.db).await, [("local/json".to_string(), Some("designer".to_string()), false)], "no key");

    let usage_page = admin.get("/usage").await.body;
    assert!(usage_page.contains("<code>designer</code> <span class=\"hint\">on local/json</span>"), "{usage_page}");
    s.db.drop().await;
}
