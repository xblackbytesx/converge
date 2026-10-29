//! The Playground (M8): chat columns in the browser, each request run
//! through `proxy::dispatch` exactly like `/v1`. Everyone may use it with
//! their own models, personas and limits; persona drafts and the request
//! body as sent upstream are admin only. Nothing is stored here: the
//! conversation lives in the page (decision 6), usage is recorded as for
//! any request, without a key.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use askama::Template;
use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use converge_core::openai::Endpoint;
use converge_core::persona::{Param, Params, PromptMode};
use futures_util::StreamExt as _;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::session::{AdminUser, WebUser};
use super::{Nav, html};
use crate::AppState;
use crate::error::{ApiError, WebError};
use crate::personas::{self as store, By};
use crate::proxy::{self, Caller, Extras, Sent};
use crate::snapshot::{PersonaEntry, Snapshot};

/// Request bodies: whole conversations, so more than the website's 64 KiB.
pub const BODY_LIMIT: usize = 4 * 1024 * 1024;
/// The largest error answer read back to show it.
const ERROR_LIMIT: usize = 1024 * 1024;

#[derive(Template)]
#[template(path = "playground.html")]
struct PlaygroundPage {
    nav: Nav,
    /// Models, personas and the parameter list for `playground.js`, as JSON
    /// in a data attribute (escaped like any attribute; no inline script).
    data: String,
}

#[derive(Deserialize)]
pub struct PageQuery {
    /// A model or persona id to put in the first column.
    pick: Option<String>,
}

pub async fn page(State(st): State<AppState>, user: WebUser, Query(q): Query<PageQuery>) -> Result<Response, WebError> {
    let snap = st.gateway.load();
    let admin = user.is_admin();
    let models: Vec<Value> = snap
        .visible(user.id, None)
        .into_iter()
        .map(|m| json!({"id": m.public_id, "uuid": m.id, "connection": m.connection.name}))
        .collect();
    let personas: Vec<Value> = snap
        .visible_personas(user.id, None)
        .into_iter()
        .map(|p| {
            // Users never learn which model a persona runs on.
            let model = admin.then(|| p.model.public_id.clone());
            json!({"id": p.public_id, "uuid": p.id, "title": p.title, "model": model})
        })
        .collect();
    let params: Vec<Value> = Param::ALL.into_iter().map(|p| json!({"key": p.key(), "hint": p.hint()})).collect();
    let data = json!({
        "admin": admin,
        "pick": q.pick.unwrap_or_default(),
        "models": models,
        "personas": personas,
        "params": params,
    });
    html(PlaygroundPage { nav: Nav::new(&user, "playground"), data: data.to_string() })
}

#[derive(Deserialize)]
struct Message {
    role: String,
    content: String,
}

/// An admin's unsaved persona. `params` are the persona form's own fields
/// (`temperature_mode`, `temperature_value`, ...).
#[derive(Deserialize)]
pub struct Draft {
    persona: Uuid,
    model: Uuid,
    prompt: String,
    mode: String,
    #[serde(default)]
    params: BTreeMap<String, String>,
}

#[derive(Deserialize)]
pub struct SendReq {
    /// A model or persona id, as a client sends `model`.
    target: String,
    messages: Vec<Message>,
    /// The column's own system prompt, sent as a client's.
    #[serde(default)]
    system: String,
    /// The column's own parameters, by key, as typed.
    #[serde(default)]
    params: BTreeMap<String, String>,
    draft: Option<Draft>,
}

fn draft_params(d: &Draft) -> Result<Params, ApiError> {
    Params::from_form(|k| d.params.get(k).map(String::as_str), true).map_err(ApiError::bad)
}

/// A draft as a one-off persona entry: the saved persona's identity with
/// the draft's model, prompt and parameters. The snapshot is not touched.
async fn draft_entry(st: &AppState, snap: &Snapshot, d: &Draft) -> Result<Arc<PersonaEntry>, ApiError> {
    let p = store::get(&st.pool, d.persona).await?.ok_or(ApiError::NotFound)?;
    let model = snap
        .models
        .values()
        .find(|m| m.id == d.model)
        .cloned()
        .ok_or_else(|| ApiError::bad("that model, or its connection, is off"))?;
    Ok(Arc::new(PersonaEntry {
        id: p.id,
        public_id: p.public_id,
        title: p.title,
        description: p.description,
        // Only `/v1/models` reads it, and a draft is never listed.
        created: 0,
        model,
        prompt: store::check_prompt(&d.prompt).map_err(|e| ApiError::bad(e.to_string()))?,
        mode: PromptMode::parse(&d.mode),
        params: draft_params(d)?,
    }))
}

/// One turn of one column: the conversation so far, streamed back as the
/// upstream's chat chunks after a first `event: converge` with what the
/// request became. Anything else (errors, an upstream that answered whole)
/// comes back as JSON with that same information.
pub async fn send(State(st): State<AppState>, user: WebUser, Json(req): Json<SendReq>) -> Result<Response, ApiError> {
    let start = Instant::now();
    let admin = user.is_admin();
    if req.messages.is_empty() || req.messages.last().is_some_and(|m| m.role != "user") {
        return Err(ApiError::bad("the conversation has to end with your message"));
    }
    if req.messages.iter().any(|m| m.role != "user" && m.role != "assistant") {
        return Err(ApiError::bad("messages are from user or assistant"));
    }
    let snap = st.gateway.load();
    let draft = match &req.draft {
        Some(_) if !admin => return Err(ApiError::Forbidden("only an admin can try a persona draft".into())),
        Some(d) => Some(draft_entry(&st, &snap, d).await?),
        None => None,
    };
    let target = draft.as_ref().map_or(req.target.clone(), |d| d.public_id.clone());

    // The column's settings are what a client would send. The token field
    // follows the connection, as a client configured for it would.
    let client = Params::from_form(|k| req.params.get(k.trim_end_matches("_value")).map(String::as_str), false)
        .map_err(ApiError::bad)?;
    let token_field = draft
        .as_ref()
        .map(|d| d.model.connection.token_field)
        .or_else(|| snap.models.get(&target).map(|m| m.connection.token_field))
        .or_else(|| snap.personas.get(&target).map(|p| p.model.connection.token_field))
        .unwrap_or("max_tokens");
    let mut messages: Vec<Value> = Vec::with_capacity(req.messages.len() + 1);
    if !req.system.trim().is_empty() {
        messages.push(json!({"role": "system", "content": req.system}));
    }
    messages.extend(req.messages.iter().map(|m| json!({"role": m.role, "content": m.content})));
    let mut body = json!({
        "model": target,
        "stream": true,
        "stream_options": {"include_usage": true},
        "messages": messages,
    });
    for (p, s) in &client.0 {
        let key = if *p == Param::MaxTokens { token_field } else { p.key() };
        body[key] = s.value.clone();
    }

    let caller = Caller { principal: user.id, key: None, label: "web".into(), owner: user.email.clone(), scope: None };
    let extras = Extras { accept: None, inspect: true, draft };
    let resp = proxy::dispatch(&st, snap, caller, body, Endpoint::Chat, extras, start).await;
    let info = resp.extensions().get::<Sent>().map(|s| describe(s, admin)).unwrap_or(Value::Null);

    let status = resp.status();
    let sse = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c.starts_with("text/event-stream"));
    if status.is_success() && sse {
        let first = Bytes::from(format!("event: converge\ndata: {info}\n\n"));
        let rest = resp.into_body().into_data_stream().map(|r| r.map_err(std::io::Error::other));
        let stream = futures_util::stream::once(async move { Ok::<_, std::io::Error>(first) }).chain(rest);
        return Ok(([(CONTENT_TYPE, "text/event-stream"), (CACHE_CONTROL, "no-cache")], Body::from_stream(stream))
            .into_response());
    }
    let bytes = axum::body::to_bytes(resp.into_body(), ERROR_LIMIT).await.unwrap_or_default();
    let answer: Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!({"error": {"message": String::from_utf8_lossy(&bytes)}}));
    // A 2xx here is an upstream that answered whole although asked to
    // stream: the page shows it like a streamed answer.
    Ok((status, Json(json!({"converge": info, "status": status.as_u16(), "answer": answer}))).into_response())
}

/// What the page shows about a request. Users see which persona answered
/// and what was dropped, never the model behind a persona or the body.
fn describe(s: &Sent, admin: bool) -> Value {
    if admin {
        let sent: Value = serde_json::from_slice(&s.body).unwrap_or(Value::Null);
        return json!({
            "model": s.model,
            "upstream_model": s.upstream_model,
            "connection": s.connection,
            "persona": s.persona,
            "dropped": s.dropped,
            "sent": sent,
        });
    }
    let model = s.persona.is_none().then(|| s.model.clone());
    json!({"model": model, "persona": s.persona, "dropped": s.dropped})
}

/// A persona as it is saved, to start a draft from.
pub async fn persona(State(st): State<AppState>, _a: AdminUser, Path(id): Path<Uuid>) -> Result<Response, ApiError> {
    let p = store::get(&st.pool, id).await?.ok_or(ApiError::NotFound)?;
    let saved = Params::from_json(&p.params);
    let mut params = serde_json::Map::new();
    for k in Param::ALL {
        if let Some(s) = saved.get(k) {
            params.insert(format!("{}_mode", k.key()), Value::from(s.mode.as_str()));
            params.insert(format!("{}_value", k.key()), Value::from(Param::show(&s.value)));
        }
    }
    Ok(Json(json!({
        "persona": p.id,
        "model": p.model_id,
        "prompt": p.system_prompt,
        "mode": p.prompt_mode,
        "params": params,
    }))
    .into_response())
}

/// Saves a draft through the normal persona update, so it lands in the
/// persona's history and can be undone there. Title, id, description,
/// visibility and the on switch stay as they are.
pub async fn save(
    State(st): State<AppState>,
    AdminUser(me): AdminUser,
    Path(id): Path<Uuid>,
    Json(d): Json<Draft>,
) -> Result<Response, ApiError> {
    if d.persona != id {
        return Err(ApiError::bad("the draft belongs to another persona"));
    }
    let p = store::get(&st.pool, id).await?.ok_or(ApiError::NotFound)?;
    let mut input = p.input();
    input.model_id = d.model;
    input.system_prompt = d.prompt.clone();
    input.prompt_mode = PromptMode::parse(&d.mode);
    input.params = draft_params(&d)?;
    store::update(&st.pool, By { id: me.id, name: &me.email }, id, &input)
        .await
        .map_err(|e| ApiError::bad(e.to_string()))?;
    st.reload().await?;
    Ok(Json(json!({"saved": true})).into_response())
}
