//! `/v1`: the OpenAI-compatible endpoint. Everything it needs comes from the
//! snapshot; the only IO is the upstream call and a usage record handed to
//! the writer (AGENTS.md, Request path and M2 plan).

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::header::{ACCEPT, AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use chrono::Utc;
use converge_core::anthropic;
use converge_core::openai::{self, Endpoint, Usage};
use converge_core::persona;
use converge_core::responses::{self, Meta};
use converge_core::secrets::{KEY_PREFIX, is_shaped};
use converge_core::sse::Splitter;
use futures_util::stream::{BoxStream, StreamExt};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::AppState;
use crate::auth::secret_hash;
use crate::connections::Kind;
use crate::error::ApiError;
use crate::limits::{ApiLimiter, Permit};
use crate::metrics::{self, Metrics};
use crate::snapshot::{KeyEntry, ModelEntry, PersonaEntry, Scope, Snapshot};
use crate::usage::{Record, UsageSink};

/// Request bodies: room for images in messages.
pub const BODY_LIMIT: usize = 32 * 1024 * 1024;
/// How much of a JSON answer is kept to read its usage. Larger answers still
/// pass through whole; only their usage goes unread.
const JSON_TAP_LIMIT: usize = 16 * 1024 * 1024;

const ENDPOINTS: [Endpoint; 7] = [
    Endpoint::Chat,
    Endpoint::Completions,
    Endpoint::Embeddings,
    Endpoint::Rerank,
    Endpoint::Images,
    Endpoint::Speech,
    Endpoint::Responses,
];

pub fn routes() -> Router<AppState> {
    let mut r = Router::new()
        .route("/v1/models", get(list_models))
        .route("/v1/models/{*id}", get(get_model))
        .route("/v1/responses/{id}", get(stored).delete(stored))
        .route("/v1/responses/{id}/cancel", post(stored))
        .route("/v1/responses/{id}/input_items", get(stored));
    for e in ENDPOINTS {
        r = r.route(
            &format!("/v1/{}", e.path()),
            post(move |st: State<AppState>, h: HeaderMap, b: Bytes| forward(st, h, b, e)),
        );
    }
    r.layer(DefaultBodyLimit::max(BODY_LIMIT))
}

/// Who a request is for: an API key, or an admin trying a persona on the
/// website (no key; still recorded as usage).
#[derive(Debug, Clone)]
pub struct Caller {
    pub principal: Uuid,
    pub key: Option<Uuid>,
    /// The key's display prefix, or `web`, for the log line.
    pub label: String,
    pub owner: String,
    /// A scoped key's models and personas.
    pub scope: Option<Arc<Scope>>,
}

impl From<KeyEntry> for Caller {
    fn from(k: KeyEntry) -> Self {
        Caller { principal: k.principal, key: Some(k.id), label: k.prefix, owner: k.owner, scope: k.scope }
    }
}

/// The key in `Authorization: Bearer cvg_...`, checked against the snapshot.
/// The shape is checked first, so garbage costs no hashing.
fn authenticate(headers: &HeaderMap, snap: &Snapshot) -> Result<KeyEntry, ApiError> {
    let value = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()).ok_or(ApiError::Unauthorized)?;
    let (scheme, token) = value.split_once(' ').ok_or(ApiError::Unauthorized)?;
    if !scheme.eq_ignore_ascii_case("bearer") || !is_shaped(token.trim(), KEY_PREFIX) {
        return Err(ApiError::Unauthorized);
    }
    let key = snap.keys.get(&secret_hash(token.trim())).ok_or(ApiError::Unauthorized)?;
    if key.expires_at.is_some_and(|t| t <= Utc::now()) {
        return Err(ApiError::Unauthorized);
    }
    Ok(key.clone())
}

fn model_json(m: &ModelEntry) -> Value {
    json!({
        "id": m.public_id,
        "object": "model",
        "created": m.created,
        "owned_by": m.connection.name,
        "converge": { "kind": "model" },
    })
}

/// Never names the model behind it: that is the admin's to swap.
fn persona_json(p: &PersonaEntry) -> Value {
    json!({
        "id": p.public_id,
        "object": "model",
        "created": p.created,
        "owned_by": "persona",
        "converge": { "kind": "persona", "title": p.title, "description": p.description },
    })
}

/// Models first, then personas, each sorted by id.
async fn list_models(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let snap = st.gateway.load();
    let key = authenticate(&headers, &snap)?;
    let scope = key.scope.as_deref();
    let models = snap.visible(key.principal, scope).into_iter().map(|m| model_json(m));
    let personas = snap.visible_personas(key.principal, scope).into_iter().map(|p| persona_json(p));
    let data: Vec<Value> = models.chain(personas).collect();
    Ok(axum::Json(json!({ "object": "list", "data": data })).into_response())
}

/// Stored-response routes (retrieve, delete, cancel, input items) carry no
/// `model`, so Converge cannot tell which connection holds the response.
/// Not served (M4 plan); the key is still checked first.
async fn stored(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let snap = st.gateway.load();
    authenticate(&headers, &snap)?;
    Err(ApiError::bad(
        "Converge does not keep or route stored responses: send the whole conversation in `input` \
         (with `store: false`)",
    ))
}

/// A Responses id for a translated answer: `resp_` and 128 random bits.
fn response_id() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("OS randomness");
    format!("resp_{}", hex::encode(b))
}

/// What a public id names for this principal, through the key's scope: a
/// persona (with its model) or a model. Unknown, not granted and out of
/// scope look the same.
fn resolve(
    snap: &Snapshot,
    principal: Uuid,
    scope: Option<&Scope>,
    id: &str,
) -> Option<(Arc<ModelEntry>, Option<Arc<PersonaEntry>>)> {
    if let Some(p) = snap
        .personas
        .get(id)
        .filter(|p| snap.persona_allowed(principal, p.id) && scope.is_none_or(|s| s.personas.contains(&p.id)))
    {
        return Some((p.model.clone(), Some(p.clone())));
    }
    snap.models
        .get(id)
        .filter(|m| snap.allowed(principal, m.id) && scope.is_none_or(|s| s.models.contains(&m.id)))
        .map(|m| (m.clone(), None))
}

async fn get_model(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let snap = st.gateway.load();
    let key = authenticate(&headers, &snap)?;
    let body = match resolve(&snap, key.principal, key.scope.as_deref(), &id).ok_or(ApiError::ModelNotFound(id))? {
        (_, Some(p)) => persona_json(&p),
        (m, None) => model_json(&m),
    };
    Ok(axum::Json(body).into_response())
}

async fn forward(State(st): State<AppState>, headers: HeaderMap, body: Bytes, endpoint: Endpoint) -> Response {
    let start = Instant::now();
    let snap = st.gateway.load();
    let caller: Caller = match authenticate(&headers, &snap) {
        Ok(k) => k.into(),
        Err(e) => return e.into_response(),
    };
    let json: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return ApiError::bad("the request body is not valid JSON").into_response(),
    };
    let extras = Extras { accept: headers.get(ACCEPT).cloned(), ..Default::default() };
    dispatch(&st, snap, caller, json, endpoint, extras, start).await
}

/// What a website caller (the Playground) can add to a request.
#[derive(Default)]
pub struct Extras {
    pub accept: Option<HeaderValue>,
    /// Hand back what went upstream as a [`Sent`] response extension.
    pub inspect: bool,
    /// An admin's unsaved persona: used instead of resolving `model`.
    pub draft: Option<Arc<PersonaEntry>>,
}

/// What a request turned into on its way upstream. A response extension,
/// never on the wire; only asked for by the Playground.
#[derive(Debug, Clone)]
pub struct Sent {
    /// The body exactly as sent (after persona and translation).
    pub body: Bytes,
    /// The model that answered (also when a persona was asked for).
    pub model: String,
    pub upstream_model: String,
    pub connection: String,
    pub persona: Option<String>,
    /// Fields that upstream cannot take, left out.
    pub dropped: Vec<String>,
}

/// One request to the upstream, from a parsed body: resolve the model or
/// persona, rewrite, send, and stream the answer back through the tap.
/// The website's Playground comes in here too.
pub async fn dispatch(
    st: &AppState,
    snap: Arc<Snapshot>,
    caller: Caller,
    json: Value,
    endpoint: Endpoint,
    extras: Extras,
    start: Instant,
) -> Response {
    let mut sent = None;
    let mut r = run(st, snap, caller, json, endpoint, extras, start, &mut sent).await;
    if let Some(s) = sent {
        r.extensions_mut().insert(s);
    }
    r
}

#[allow(clippy::too_many_arguments)]
async fn run(
    st: &AppState,
    snap: Arc<Snapshot>,
    caller: Caller,
    mut json: Value,
    endpoint: Endpoint,
    extras: Extras,
    start: Instant,
    sent: &mut Option<Sent>,
) -> Response {
    let Extras { accept, inspect, draft } = extras;
    let name = match openai::model(&json) {
        Ok(n) => n.to_string(),
        Err(e) => return ApiError::bad(e.to_string()).into_response(),
    };
    let found = match draft {
        Some(d) => Some((d.model.clone(), Some(d))),
        None => resolve(&snap, caller.principal, caller.scope.as_deref(), &name),
    };
    let Some((model, persona)) = found else {
        return ApiError::ModelNotFound(name).into_response();
    };
    let limits = snap.limits.get(&caller.principal).copied().unwrap_or_default();
    // A long answer must not keep an old snapshot alive.
    drop(snap);
    if persona.is_some() && !matches!(endpoint, Endpoint::Chat | Endpoint::Responses) {
        return ApiError::bad(format!(
            "`{name}` is a persona; personas work with /v1/chat/completions and /v1/responses, not /v1/{}",
            endpoint.path()
        ))
        .into_response();
    }
    let conn = model.connection.clone();
    let claude = conn.kind == Kind::Anthropic;
    if claude && !matches!(endpoint, Endpoint::Chat | Endpoint::Responses) {
        return ApiError::bad(format!(
            "`{name}` is on an Anthropic connection, which serves /v1/chat/completions and /v1/responses, not /v1/{}",
            endpoint.path()
        ))
        .into_response();
    }

    // Responses on a translating connection goes upstream as chat
    // completions, and its answer comes back through `responses`.
    let mut translation: Option<(Meta, Vec<String>)> = None;
    if endpoint == Endpoint::Responses && !conn.native_responses {
        match responses::to_chat(&json, conn.token_field) {
            Ok(t) => {
                let meta = Meta {
                    id: response_id(),
                    created_at: Utc::now().timestamp(),
                    model: name.clone(),
                    reasoning: model.reasoning_items,
                    echo: t.echo,
                };
                translation = Some((meta, t.dropped));
                json = t.chat;
            }
            Err(e) => return ApiError::bad(e).into_response(),
        }
    }
    let wire = if translation.is_some() { Endpoint::Chat } else { endpoint };

    let prep = openai::prepare(&mut json, wire, &model.upstream_name);
    if let Some(obj) = json.as_object_mut() {
        match (&persona, wire) {
            (Some(p), Endpoint::Responses) => {
                responses::apply_native_prompt(obj, &p.prompt, p.mode);
                responses::fill_native(obj, Some(&p.params), &model.params);
            }
            (Some(p), _) => {
                persona::apply_prompt(obj, &p.prompt, p.mode);
                persona::fill(obj, Some(&p.params), &model.params, conn.token_field);
            }
            (None, _) if model.params.is_empty() => {}
            (None, Endpoint::Responses) => responses::fill_native(obj, None, &model.params),
            (None, Endpoint::Chat | Endpoint::Completions) => {
                persona::fill(obj, None, &model.params, conn.token_field);
            }
            (None, _) => {}
        }
    }
    // Anthropic: the chat body (persona applied) becomes a Messages body.
    let mut dropped: Vec<String> = translation.as_ref().map(|(_, d)| d.clone()).unwrap_or_default();
    if claude {
        let opts = anthropic::Opts { thinking: model.reasoning_items, cache: model.prompt_cache };
        match anthropic::to_messages(&json, opts) {
            Ok(t) => {
                json = t.body;
                dropped.extend(t.dropped);
            }
            Err(e) => return ApiError::bad(e).into_response(),
        }
    }

    // Limits last: a request refused for its content uses up nothing.
    let permit = match st.limiter.check(caller.principal, caller.key, &limits, Instant::now(), Utc::now()) {
        Ok(p) => p,
        Err(refused) => {
            let mut g = UsageGuard::new(st, caller, &model, persona.as_deref(), endpoint, start, &limits, None);
            g.status = StatusCode::TOO_MANY_REQUESTS;
            g.limited = Some(refused.kind());
            return ApiError::RateLimited { message: refused.message(), retry_after: refused.retry_after() }
                .into_response();
        }
    };
    let mut guard = UsageGuard::new(st, caller, &model, persona.as_deref(), endpoint, start, &limits, Some(permit));
    if conn.unreadable {
        guard.status = StatusCode::BAD_GATEWAY;
        guard.broke = Some("unreadable");
        return ApiError::Upstream(format!(
            "the stored credential of connection `{}` cannot be read; an admin has to enter it again",
            conn.name
        ))
        .into_response();
    }
    let out = serde_json::to_vec(&json).expect("a parsed Value serializes");
    dropped.sort();
    dropped.dedup();
    if inspect {
        *sent = Some(Sent {
            body: Bytes::from(out.clone()),
            model: model.public_id.clone(),
            upstream_model: model.upstream_name.clone(),
            connection: conn.name.clone(),
            persona: persona.as_ref().map(|p| p.public_id.clone()),
            dropped: dropped.clone(),
        });
    }

    // Only what the upstream needs: the body, its type, what the client
    // accepts, and this connection's own credential and headers. The
    // client's key, cookies and other headers stay here.
    let mut req = st
        .http
        .post(format!("{}/{}", conn.base_url, if claude { "messages" } else { wire.path() }))
        .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
        .body(out);
    if let Some(accept) = accept {
        req = req.header(ACCEPT, accept);
    }
    if let Some((n, v)) = &conn.auth {
        req = req.header(n.clone(), v.clone());
    }
    if claude {
        req = req.header("anthropic-version", anthropic::VERSION);
    }
    for (n, v) in &conn.extra_headers {
        req = req.header(n.clone(), v.clone());
    }

    let resp = match tokio::time::timeout(conn.first_byte, req.send()).await {
        Err(_) => {
            guard.status = StatusCode::GATEWAY_TIMEOUT;
            guard.broke = Some("timeout");
            return ApiError::UpstreamTimeout(format!(
                "connection `{}` did not answer within {} s",
                conn.name,
                conn.first_byte.as_secs()
            ))
            .into_response();
        }
        Ok(Err(e)) => {
            guard.status = StatusCode::BAD_GATEWAY;
            guard.broke = Some(if e.is_connect() { "connect" } else { "request" });
            let why = if e.is_connect() { "could not connect" } else { "the request failed" };
            return ApiError::Upstream(format!("connection `{}`: {why}", conn.name)).into_response();
        }
        Ok(Ok(r)) => r,
    };

    let status = resp.status();
    guard.status = status;
    let mut content_type = resp.headers().get(CONTENT_TYPE).cloned();
    let ct = content_type.as_ref().and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let dropped = Some(dropped.join(", ")).filter(|d| !d.is_empty());
    let dropped = dropped.and_then(|d| HeaderValue::from_str(&d).ok());
    let with_dropped = |mut r: Response| {
        if let Some(d) = &dropped {
            r.headers_mut().insert("converge-dropped-params", d.clone());
        }
        r
    };

    let mut out = std::collections::VecDeque::new();
    let meta = translation.map(|(m, _)| m);
    let mode = if claude {
        let sse = ct.starts_with("text/event-stream");
        if status.is_success() && sse {
            let then = meta.map(|m| {
                let mut s = Box::new(responses::Stream::new(m));
                out.push_back(Bytes::from(s.start()));
                s
            });
            let conv = Box::new(anthropic::Stream::new(&name, Utc::now().timestamp()));
            Mode::Anthropic { split: Splitter::default(), conv, then, drop_usage_chunk: prep.injected_usage }
        } else {
            // A whole answer or an error: read it, translate it, answer.
            let body = match read_all(resp, conn.chunk, &mut guard).await {
                Ok(b) => b,
                Err(why) => {
                    guard.broke_off(why);
                    guard.status = StatusCode::BAD_GATEWAY;
                    return ApiError::Upstream(format!("connection `{}` {why}", conn.name)).into_response();
                }
            };
            guard.finished = true;
            if !status.is_success() {
                let (code, err) = anthropic::error_to_openai(status.as_u16(), &body);
                let code = StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY);
                return with_dropped((code, axum::Json(err)).into_response());
            }
            let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
                guard.status = StatusCode::BAD_GATEWAY;
                return ApiError::Upstream(format!("connection `{}` answered something other than JSON", conn.name))
                    .into_response();
            };
            let chat = anthropic::from_message(&msg, &name, Utc::now().timestamp());
            guard.usage = openai::usage(&chat);
            let answer = match &meta {
                Some(m) => responses::from_chat(&chat, m),
                None => chat,
            };
            return with_dropped(axum::Json(answer).into_response());
        }
    } else if let Some(meta) = meta.filter(|_| status.is_success()) {
        if ct.starts_with("text/event-stream") {
            let mut conv = Box::new(responses::Stream::new(meta));
            out.push_back(Bytes::from(conv.start()));
            Mode::Translate { split: Splitter::default(), conv }
        } else {
            // Not streamed: the whole answer is needed to build the Response.
            let r = match read_all(resp, conn.chunk, &mut guard).await {
                Ok(b) => serde_json::from_slice::<Value>(&b).map_err(|_| "answered something other than JSON"),
                Err(why) => Err(why),
            };
            let chat = match r {
                Ok(v) => v,
                Err(why) => {
                    guard.broke_off(why);
                    guard.status = StatusCode::BAD_GATEWAY;
                    return ApiError::Upstream(format!("connection `{}` {why}", conn.name)).into_response();
                }
            };
            guard.usage = openai::usage(&chat);
            guard.finished = true;
            return with_dropped(axum::Json(responses::from_chat(&chat, &meta)).into_response());
        }
    } else if ct.starts_with("text/event-stream") {
        Mode::Sse { split: Splitter::default(), drop_usage_chunk: prep.injected_usage }
    } else if ct.starts_with("application/json") && status.is_success() {
        Mode::Json(Vec::new())
    } else {
        Mode::Raw
    };
    let sse = matches!(mode, Mode::Sse { .. } | Mode::Translate { .. } | Mode::Anthropic { .. });
    if matches!(mode, Mode::Translate { .. } | Mode::Anthropic { .. }) {
        content_type = Some(HeaderValue::from_static("text/event-stream"));
    }
    let tap = Tap { upstream: resp.bytes_stream().boxed(), mode, guard, chunk: conn.chunk, out, done: false };

    let mut r = with_dropped(Response::new(Body::from_stream(tap.into_stream())));
    *r.status_mut() = status;
    if let Some(ct) = content_type {
        r.headers_mut().insert(CONTENT_TYPE, ct);
    }
    if sse {
        r.headers_mut().insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        // Proxies in front (nginx and friends) must not buffer the stream.
        r.headers_mut().insert("x-accel-buffering", HeaderValue::from_static("no"));
    }
    r
}

enum Mode {
    Sse {
        split: Splitter,
        drop_usage_chunk: bool,
    },
    /// A successful JSON answer, kept (up to the limit) to read its usage.
    Json(Vec<u8>),
    /// A chat stream turned into Responses events as it arrives.
    Translate {
        split: Splitter,
        conv: Box<responses::Stream>,
    },
    /// An Anthropic stream turned into chat chunks, and on into Responses
    /// events when `then` is set.
    Anthropic {
        split: Splitter,
        conv: Box<anthropic::Stream>,
        then: Option<Box<responses::Stream>>,
        drop_usage_chunk: bool,
    },
    /// Errors, audio, anything else: passed through, not read.
    Raw,
}

/// The response body on its way through. Ends when the upstream ends; a
/// stalled or broken upstream ends it with an error, which aborts the
/// response so the client sees a cut, never a silently short answer.
struct Tap {
    upstream: BoxStream<'static, reqwest::Result<Bytes>>,
    mode: Mode,
    guard: UsageGuard,
    chunk: Duration,
    out: std::collections::VecDeque<Bytes>,
    /// The upstream ended; hand out what is left in `out`, then stop.
    done: bool,
}

impl Tap {
    fn into_stream(self) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
        futures_util::stream::unfold(Some(self), |state| async move {
            let mut t = state?;
            loop {
                if let Some(b) = t.out.pop_front() {
                    return Some((Ok(b), Some(t)));
                }
                if t.done {
                    return None;
                }
                match tokio::time::timeout(t.chunk, t.upstream.next()).await {
                    Err(_) => {
                        t.guard.broke_off("stalled");
                        return Some((Err(std::io::Error::other("upstream stalled")), None));
                    }
                    Ok(Some(Err(_))) => {
                        t.guard.broke_off("broke off");
                        return Some((Err(std::io::Error::other("upstream broke off")), None));
                    }
                    Ok(Some(Ok(bytes))) => {
                        t.guard.first_byte();
                        t.take(bytes);
                    }
                    Ok(None) => {
                        t.end();
                        t.done = true;
                    }
                }
            }
        })
    }

    fn take(&mut self, bytes: Bytes) {
        match &mut self.mode {
            Mode::Raw => self.out.push_back(bytes),
            Mode::Json(buf) => {
                if buf.len() + bytes.len() <= JSON_TAP_LIMIT {
                    buf.extend_from_slice(&bytes);
                } else {
                    buf.clear();
                    buf.shrink_to_fit();
                    self.mode = Mode::Raw;
                }
                self.out.push_back(bytes);
            }
            Mode::Translate { split, conv } => {
                for event in split.push(&bytes) {
                    translate_event(&event, conv, &mut self.guard, &mut self.out);
                }
            }
            Mode::Anthropic { split, conv, then, drop_usage_chunk } => {
                for event in split.push(&bytes) {
                    anthropic_event(
                        &event,
                        conv,
                        then.as_deref_mut(),
                        *drop_usage_chunk,
                        &mut self.guard,
                        &mut self.out,
                    );
                }
            }
            Mode::Sse { split, drop_usage_chunk } => {
                for event in split.push(&bytes) {
                    if let Some(v) = openai::event_json(&event)
                        && let Some(u) = openai::stream_usage(&v)
                    {
                        self.guard.usage = Some(u);
                        if *drop_usage_chunk && openai::is_usage_only_chunk(&v) {
                            continue;
                        }
                    }
                    self.out.push_back(Bytes::from(event));
                }
            }
        }
    }

    fn end(&mut self) {
        match &mut self.mode {
            Mode::Sse { split, .. } => {
                if let Some(rest) = split.finish() {
                    self.out.push_back(Bytes::from(rest));
                }
            }
            Mode::Translate { split, conv } => {
                if let Some(rest) = split.finish() {
                    translate_event(&rest, conv, &mut self.guard, &mut self.out);
                }
                self.out.push_back(Bytes::from(conv.end()));
            }
            Mode::Anthropic { split, conv, then, drop_usage_chunk } => {
                if let Some(rest) = split.finish() {
                    anthropic_event(
                        &rest,
                        conv,
                        then.as_deref_mut(),
                        *drop_usage_chunk,
                        &mut self.guard,
                        &mut self.out,
                    );
                }
                // Only a stream that reached `message_stop` is complete; a
                // cut one ends without `[DONE]`, as the upstream ended.
                if conv.ended() {
                    match then {
                        Some(r) => self.out.push_back(Bytes::from(r.end())),
                        None => self.out.push_back(Bytes::from_static(b"data: [DONE]\n\n")),
                    }
                }
            }
            Mode::Json(buf) => {
                self.guard.usage = serde_json::from_slice::<Value>(buf).ok().and_then(|v| openai::usage(&v));
            }
            Mode::Raw => {}
        }
        self.guard.finished = true;
    }
}

/// One chat chunk through the translation; usage is read on the way.
fn translate_event(
    event: &[u8],
    conv: &mut responses::Stream,
    guard: &mut UsageGuard,
    out: &mut std::collections::VecDeque<Bytes>,
) {
    let Some(v) = openai::event_json(event) else { return };
    if let Some(u) = openai::usage(&v) {
        guard.usage = Some(u);
    }
    let b = conv.push(&v);
    if !b.is_empty() {
        out.push_back(Bytes::from(b));
    }
}

/// One Anthropic event: to chat chunks, then to the client as chat
/// (`data:` lines) or on through the Responses translation. Usage is read
/// from the chat chunks.
fn anthropic_event(
    event: &[u8],
    conv: &mut anthropic::Stream,
    then: Option<&mut responses::Stream>,
    drop_usage_chunk: bool,
    guard: &mut UsageGuard,
    out: &mut std::collections::VecDeque<Bytes>,
) {
    let Some(v) = openai::event_json(event) else { return };
    let chunks = conv.push(&v);
    match then {
        Some(r) => {
            for c in &chunks {
                if let Some(u) = openai::usage(c) {
                    guard.usage = Some(u);
                }
                let b = r.push(c);
                if !b.is_empty() {
                    out.push_back(Bytes::from(b));
                }
            }
        }
        None => {
            for c in &chunks {
                if let Some(u) = openai::usage(c) {
                    guard.usage = Some(u);
                    if drop_usage_chunk && openai::is_usage_only_chunk(c) {
                        continue;
                    }
                }
                out.push_back(Bytes::from(format!("data: {c}\n\n")));
            }
        }
    }
}

/// A whole answer, with the same chunk timer as streams and the tap's size
/// limit. The error says what went wrong, for the 502.
async fn read_all(resp: reqwest::Response, chunk: Duration, guard: &mut UsageGuard) -> Result<Vec<u8>, &'static str> {
    let mut s = resp.bytes_stream();
    let mut buf = Vec::new();
    loop {
        match tokio::time::timeout(chunk, s.next()).await {
            Err(_) => return Err("stalled"),
            Ok(Some(Err(_))) => return Err("broke off"),
            Ok(Some(Ok(b))) => {
                guard.first_byte();
                if buf.len() + b.len() > JSON_TAP_LIMIT {
                    return Err("answered more than Converge translates");
                }
                buf.extend_from_slice(&b);
            }
            Ok(None) => return Ok(buf),
        }
    }
}

/// Writes the usage record and the log line when the request is over, in
/// every way it can be over: finished, failed, or dropped because the
/// client went away (which also drops the upstream connection, so
/// generation stops there).
struct UsageGuard {
    sink: UsageSink,
    limiter: Arc<ApiLimiter>,
    metrics: Arc<Metrics>,
    tz: chrono_tz::Tz,
    /// The place on the key, freed when the request is over. None: turned
    /// away by a limit, so never in flight either.
    permit: Option<Permit>,
    limited: Option<&'static str>,
    caller: Caller,
    model: Arc<ModelEntry>,
    persona: Option<(Uuid, String)>,
    endpoint: Endpoint,
    start: Instant,
    ttfb: Option<Duration>,
    status: StatusCode,
    usage: Option<Usage>,
    finished: bool,
    broke: Option<&'static str>,
}

impl UsageGuard {
    #[allow(clippy::too_many_arguments)]
    fn new(
        st: &AppState,
        caller: Caller,
        model: &Arc<ModelEntry>,
        persona: Option<&PersonaEntry>,
        endpoint: Endpoint,
        start: Instant,
        limits: &crate::limits::Limits,
        permit: Option<Permit>,
    ) -> Self {
        if permit.is_some() {
            st.metrics.started(&model.connection.name);
        }
        UsageGuard {
            sink: st.usage.clone(),
            limiter: st.limiter.clone(),
            metrics: st.metrics.clone(),
            tz: limits.tz,
            permit,
            limited: None,
            caller,
            model: model.clone(),
            persona: persona.map(|p| (p.id, p.public_id.clone())),
            endpoint,
            start,
            ttfb: None,
            status: StatusCode::OK,
            usage: None,
            // Early error answers are complete when they are made.
            finished: true,
            broke: None,
        }
    }

    fn first_byte(&mut self) {
        if self.ttfb.is_none() {
            self.ttfb = Some(self.start.elapsed());
            // From here on the client could go away mid-answer.
            self.finished = false;
        }
    }

    fn broke_off(&mut self, why: &'static str) {
        self.broke = Some(why);
        self.finished = true;
    }
}

/// Saturating, for the database's `integer`.
fn ms(d: Duration) -> i32 {
    i32::try_from(d.as_millis()).unwrap_or(i32::MAX)
}

impl Drop for UsageGuard {
    fn drop(&mut self) {
        let total = self.start.elapsed();
        let client_closed = !self.finished;
        let u = self.usage.unwrap_or_default();
        let now = Utc::now();
        self.limiter.add_tokens(self.caller.principal, self.tz, now, u.prompt_tokens + u.completion_tokens);
        let status = match (self.limited, self.status.as_u16()) {
            (Some(_), _) => "limited",
            (None, 200..=299) => "2xx",
            (None, 400..=499) => "4xx",
            (None, 500..=599) => "5xx",
            (None, _) => "other",
        };
        let persona = self.persona.as_ref().map_or("", |p| p.1.as_str());
        self.metrics.done(metrics::Done {
            endpoint: self.endpoint.path(),
            model: &self.model.public_id,
            persona,
            connection: &self.model.connection.name,
            status,
            prompt: u.prompt_tokens,
            completion: u.completion_tokens,
            cached: u.cached_tokens,
            ttfb: self.ttfb,
            total,
            failure: self.broke,
            limited: self.limited,
        });
        if self.permit.take().is_some() {
            self.metrics.ended(&self.model.connection.name);
        }
        tracing::info!(
            key = %self.caller.label,
            user = %self.caller.owner,
            model = %self.model.public_id,
            persona = self.persona.as_ref().map(|p| p.1.as_str()),
            status = self.status.as_u16(),
            ttfb_ms = self.ttfb.map(ms),
            total_ms = ms(total),
            prompt = u.prompt_tokens,
            completion = u.completion_tokens,
            client_closed,
            upstream = self.broke.unwrap_or("ok"),
            limited = self.limited,
            "proxied"
        );
        self.sink.record(Record {
            at: now,
            principal: self.caller.principal,
            key: self.caller.key,
            model: self.model.id,
            model_name: self.model.public_id.clone(),
            persona: self.persona.as_ref().map(|p| p.0),
            persona_name: self.persona.as_ref().map(|p| p.1.clone()),
            connection: self.model.connection.id,
            endpoint: self.endpoint.path(),
            status: self.status.as_u16(),
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
            cached_tokens: u.cached_tokens,
            ttfb_ms: self.ttfb.map(ms),
            total_ms: ms(total),
            usage_reported: self.usage.is_some(),
            client_closed,
            limited: self.limited,
        });
    }
}
