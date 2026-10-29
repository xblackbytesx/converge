//! Shared harness: a fresh database per test (dropped at the end), the real
//! router, a tiny browser that keeps the session cookie, an API client, and
//! a stand-in upstream (`upstream.rs`).
#![allow(dead_code)]

pub mod upstream;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use converge_server::auth::Role;
use converge_server::{AppState, MIGRATOR, router};
use http_body_util::BodyExt as _;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt as _;
use uuid::Uuid;

pub const PASSWORD: &str = "correct horse battery";

pub const SECRET: [u8; 32] = [9u8; 32];

pub struct Db {
    pub pool: PgPool,
    admin: PgPool,
    name: String,
    /// One state for the whole test, like the one server process: the same
    /// snapshot, usage writer and login limiter behind every router.
    state: AppState,
}

impl Db {
    /// None (and a SKIP line) when `CONVERGE_TEST_DATABASE_URL` is unset.
    pub async fn new() -> Option<Db> {
        let Ok(base) = std::env::var("CONVERGE_TEST_DATABASE_URL") else {
            eprintln!("SKIP: CONVERGE_TEST_DATABASE_URL unset");
            return None;
        };
        let admin = PgPoolOptions::new().max_connections(1).connect(&base).await.unwrap();
        let name = format!("converge_test_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {name}")).execute(&admin).await.unwrap();
        let url = format!("{}/{name}", base.rsplit_once('/').unwrap().0);
        let pool = PgPoolOptions::new().max_connections(8).connect(&url).await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let state = AppState { cookie_secure: false, ..AppState::new(pool.clone(), &SECRET) };
        state.reload().await.unwrap();
        Some(Db { pool, admin, name, state })
    }

    pub fn state(&self) -> AppState {
        self.state.clone()
    }

    pub async fn reload(&self) {
        self.state.reload().await.unwrap();
    }

    /// Waits until every usage record so far is written.
    pub async fn flush(&self) {
        self.state.usage.flush().await;
    }

    pub fn app(&self) -> Router {
        router(self.state())
    }

    /// The server restarted: usage written, then a fresh state (snapshot,
    /// limiter, metrics) on the same database, started like `serve` does.
    pub async fn restart(&mut self) {
        self.flush().await;
        let state = AppState { cookie_secure: false, ..AppState::new(self.pool.clone(), &SECRET) };
        state.start().await.unwrap();
        self.state = state;
    }

    /// A person's limits; None is no limit.
    pub async fn limits(&self, user: Uuid, rpm: Option<i32>, tokens_per_day: Option<i64>, parallel: Option<i32>) {
        sqlx::query("UPDATE users SET rpm = $2, tokens_per_day = $3, parallel_per_key = $4 WHERE id = $1")
            .bind(user)
            .bind(rpm)
            .bind(tokens_per_day)
            .bind(parallel)
            .execute(&self.pool)
            .await
            .unwrap();
        self.reload().await;
    }

    /// A key reaching only these models and personas.
    pub async fn scoped_key(&self, user: Uuid, models: &[Uuid], personas: &[Uuid]) -> String {
        let scope = converge_server::keys::KeyScope { models: models.to_vec(), personas: personas.to_vec() };
        let (_, key) = converge_server::keys::create(&self.pool, user, "scoped", 0, Some(&scope)).await.unwrap();
        self.reload().await;
        key
    }

    pub async fn drop(self) {
        self.pool.close().await;
        sqlx::query(&format!("DROP DATABASE {} WITH (FORCE)", self.name)).execute(&self.admin).await.unwrap();
    }

    /// A person with a password; returns their id.
    pub async fn person(&self, email: &str, role: Role) -> Uuid {
        let id = converge_server::users::create(&self.pool, email, role).await.unwrap();
        converge_server::users::set_password(&self.pool, email, PASSWORD).await.unwrap();
        self.reload().await;
        id
    }

    /// An `openai` connection to `base`, with `credential` when given.
    pub async fn connection(&self, slug: &str, base: &str, credential: Option<&str>) -> Uuid {
        self.connection_with(slug, base, credential, vec![], 600, 120).await
    }

    pub async fn connection_with(
        &self,
        slug: &str,
        base: &str,
        credential: Option<&str>,
        headers: Vec<(String, String)>,
        first_byte_secs: i32,
        chunk_secs: i32,
    ) -> Uuid {
        self.connection_kind(
            converge_server::connections::Kind::OpenAi,
            slug,
            base,
            credential,
            headers,
            first_byte_secs,
            chunk_secs,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn connection_kind(
        &self,
        kind: converge_server::connections::Kind,
        slug: &str,
        base: &str,
        credential: Option<&str>,
        headers: Vec<(String, String)>,
        first_byte_secs: i32,
        chunk_secs: i32,
    ) -> Uuid {
        use converge_server::connections::{ConnInput, Responses, create};
        let input = ConnInput {
            kind,
            name: format!("{slug} upstream"),
            slug: slug.into(),
            base_url: base.into(),
            credential: credential.map(str::to_string),
            headers: Some(headers),
            responses: Responses::Translate,
            token_field: "max_tokens".into(),
            first_byte_secs,
            chunk_secs,
            enabled: true,
        };
        let id = create(&self.pool, &self.state.secrets, &input).await.unwrap();
        self.reload().await;
        id
    }

    pub async fn model(&self, connection: Uuid, public_id: &str, upstream: &str) -> Uuid {
        let id = converge_server::connections::add_model(&self.pool, connection, public_id, upstream).await.unwrap();
        self.reload().await;
        id
    }

    pub async fn key(&self, user: Uuid) -> String {
        let (_, key) = converge_server::keys::create(&self.pool, user, "test", 0, None).await.unwrap();
        self.reload().await;
        key
    }

    pub async fn grant(&self, user: Uuid, all: bool, models: &[Uuid]) {
        converge_server::keys::set_grants(&self.pool, user, all, models, &[]).await.unwrap();
        self.reload().await;
    }

    pub async fn grant_personas(&self, user: Uuid, personas: &[Uuid]) {
        converge_server::keys::set_grants(&self.pool, user, false, &[], personas).await.unwrap();
        self.reload().await;
    }

    /// A persona on `model`, made by `by` (an existing user).
    pub async fn persona(
        &self,
        by: Uuid,
        public_id: &str,
        model: Uuid,
        prompt: &str,
        params: serde_json::Value,
    ) -> Uuid {
        use converge_core::persona::{Params, PromptMode};
        use converge_server::personas::{By, PersonaInput, create};
        let input = PersonaInput {
            public_id: public_id.into(),
            title: format!("The {public_id}"),
            description: "Helps.".into(),
            model_id: model,
            system_prompt: prompt.into(),
            prompt_mode: PromptMode::Prepend,
            prompt_visible: true,
            params: Params::from_json(&params),
            enabled: true,
        };
        let id = create(&self.pool, By { id: by, name: "admin@example.com" }, &input).await.unwrap();
        self.reload().await;
        id
    }
}

/// Calls `/v1` like an API client: bearer key, JSON body.
pub async fn api(app: &Router, method: &str, path: &str, key: Option<&str>, body: Option<serde_json::Value>) -> Resp {
    let mut b = Request::builder().method(method).uri(path).header(header::HOST, "test.local");
    if let Some(k) = key {
        b = b.header(header::AUTHORIZATION, format!("Bearer {k}"));
    }
    let body = match body {
        Some(v) => {
            b = b.header(header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.map(|c| c.to_bytes()).unwrap_or_default();
    Resp { status, headers, body: String::from_utf8_lossy(&bytes).into_owned() }
}

/// Like `api`, but hands back the streaming response itself.
pub async fn api_raw(app: &Router, path: &str, key: &str, body: serde_json::Value) -> axum::response::Response {
    let req = Request::post(path)
        .header(header::HOST, "test.local")
        .header(header::AUTHORIZATION, format!("Bearer {key}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, "converge_session=should-not-travel")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    app.clone().oneshot(req).await.unwrap()
}

pub struct Resp {
    pub status: StatusCode,
    pub headers: axum::http::HeaderMap,
    pub body: String,
}

/// A browser: keeps the session cookie, sends same-origin Origin on writes.
pub struct Browser {
    pub app: Router,
    pub cookie: Option<String>,
}

impl Browser {
    pub fn new(app: Router) -> Self {
        Browser { app, cookie: None }
    }

    pub async fn logged_in(app: Router, email: &str) -> Self {
        let mut b = Browser::new(app);
        let r = b.post("/login", &[("email", email), ("password", PASSWORD)]).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
        b
    }

    pub async fn get(&mut self, path: &str) -> Resp {
        self.send(Request::get(path), Body::empty(), None).await
    }

    pub async fn post(&mut self, path: &str, form: &[(&str, &str)]) -> Resp {
        self.send(
            Request::post(path).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
            Body::from(form_body(form)),
            Some("http://test.local"),
        )
        .await
    }

    /// A JSON POST from the page's own script (same origin).
    pub async fn post_json(&mut self, path: &str, body: &serde_json::Value) -> Resp {
        self.send(
            Request::post(path).header(header::CONTENT_TYPE, "application/json"),
            Body::from(serde_json::to_vec(body).unwrap()),
            Some("http://test.local"),
        )
        .await
    }

    pub async fn htmx_post(&mut self, path: &str, form: &[(&str, &str)]) -> Resp {
        self.send(
            Request::post(path)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header("hx-request", "true"),
            Body::from(form_body(form)),
            Some("http://test.local"),
        )
        .await
    }

    pub async fn send(&mut self, mut b: axum::http::request::Builder, body: Body, origin: Option<&str>) -> Resp {
        b = b.header(header::HOST, "test.local");
        if let Some(o) = origin {
            b = b.header(header::ORIGIN, o);
        }
        if let Some(c) = &self.cookie {
            b = b.header(header::COOKIE, c);
        }
        let resp = self.app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        if let Some(set) = headers.get(header::SET_COOKIE).and_then(|v| v.to_str().ok()) {
            let kv = set.split(';').next().unwrap().to_string();
            self.cookie = if kv.ends_with('=') { None } else { Some(kv) };
        }
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        Resp { status, headers, body: String::from_utf8_lossy(&bytes).into_owned() }
    }
}

pub fn form_body(form: &[(&str, &str)]) -> String {
    form.iter().map(|(k, v)| format!("{}={}", enc(k), enc(v))).collect::<Vec<_>>().join("&")
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            b' ' => "+".to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}
