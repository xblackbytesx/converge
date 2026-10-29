//! A stand-in OpenAI-style upstream on a real local port. What it does is
//! picked by the upstream model name in the request, so one server covers
//! every case the proxy has to survive. It records what it received, to
//! prove what does and does not travel upstream.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};

/// The credential the stand-in expects on `/models`.
pub const UPSTREAM_KEY: &str = "up-secret-key";

/// The exact JSON body "json" answers, to check byte-for-byte pass-through.
pub const CHAT_JSON: &str = r#"{"id":"c1","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":11,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":2}},"x_extra":true}"#;

/// What "resp-json" answers on the native `/v1/responses`.
pub const RESP_JSON: &str = r#"{"id":"resp_up1","object":"response","status":"completed","model":"gpt-x","output":[{"type":"message","id":"msg_1","role":"assistant","content":[{"type":"output_text","text":"native","annotations":[]}]}],"usage":{"input_tokens":8,"output_tokens":4,"input_tokens_details":{"cached_tokens":1}}}"#;

/// What "resp-stream" sends on the native `/v1/responses`.
pub const RESP_STREAM: &str = concat!(
    "event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_up2\",\"status\":\"in_progress\",\"output\":[]}}\n\n",
    "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"sequence_number\":1,\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"delta\":\"hey\"}\n\n",
    "event: response.completed\ndata: {\"type\":\"response.completed\",\"sequence_number\":2,\"response\":{\"id\":\"resp_up2\",\"status\":\"completed\",\"usage\":{\"input_tokens\":6,\"output_tokens\":2}}}\n\n",
);

pub const ERROR_JSON: &str = r#"{"error":{"message":"bad things","type":"invalid_request_error"}}"#;

#[derive(Clone)]
pub struct Seen {
    pub path: String,
    pub headers: HeaderMap,
    pub body: Value,
}

#[derive(Clone, Default)]
struct Shared {
    seen: Arc<Mutex<Vec<Seen>>>,
    /// Streams of "endless" that ended because the reader went away.
    dropped: Arc<AtomicUsize>,
}

pub struct Upstream {
    /// `http://127.0.0.1:port/v1`
    pub base: String,
    shared: Shared,
}

impl Upstream {
    pub async fn start() -> Upstream {
        let shared = Shared::default();
        let app = Router::new()
            .route("/v1/models", get(models))
            .route("/v1/{*rest}", post(answer))
            .with_state(shared.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Upstream { base: format!("http://{addr}/v1"), shared }
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.shared.seen.lock().unwrap().clone()
    }

    pub fn last(&self) -> Seen {
        self.seen().pop().expect("the upstream saw a request")
    }

    pub fn dropped(&self) -> usize {
        self.shared.dropped.load(Ordering::SeqCst)
    }
}

/// Anthropic's model list: `x-api-key` and `anthropic-version`, two pages.
fn anthropic_models(headers: &HeaderMap, uri: &axum::http::Uri) -> Response {
    let key = headers.get("x-api-key").and_then(|v| v.to_str().ok());
    if key != Some(UPSTREAM_KEY) || !headers.contains_key("anthropic-version") {
        return (StatusCode::UNAUTHORIZED, ERROR_JSON).into_response();
    }
    let q = uri.query().unwrap_or("");
    if q.contains("after_id=claude-b") {
        return axum::Json(
            json!({"data": [{"type": "model", "id": "claude-c"}], "has_more": false, "last_id": "claude-c"}),
        )
        .into_response();
    }
    axum::Json(json!({"data": [{"type": "model", "id": "claude-b"}, {"type": "model", "id": "claude-a"}], "has_more": true, "last_id": "claude-b"}))
        .into_response()
}

async fn models(headers: HeaderMap, uri: axum::http::Uri) -> Response {
    if headers.contains_key("x-api-key") {
        return anthropic_models(&headers, &uri);
    }
    let ok =
        headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {UPSTREAM_KEY}"));
    if !ok {
        return (StatusCode::UNAUTHORIZED, ERROR_JSON).into_response();
    }
    axum::Json(json!({"object": "list", "data": [{"id": "Beta-2"}, {"id": "alpha"}]})).into_response()
}

fn sse(events: Vec<String>) -> Response {
    let body = events.into_iter().map(|e| format!("data: {e}\n\n")).collect::<String>();
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

fn chunk(text: &str) -> String {
    json!({"object": "chat.completion.chunk", "choices": [{"index": 0, "delta": {"content": text}}]}).to_string()
}

fn delta(d: Value) -> String {
    json!({"object": "chat.completion.chunk", "choices": [{"index": 0, "delta": d}]}).to_string()
}

fn finish(reason: &str) -> String {
    json!({"object": "chat.completion.chunk", "choices": [{"index": 0, "delta": {}, "finish_reason": reason}]})
        .to_string()
}

fn usage_only() -> String {
    json!({"object": "chat.completion.chunk", "choices": [], "usage": {"prompt_tokens": 5, "completion_tokens": 3}})
        .to_string()
}

/// Sends `first`, then whatever `rest` does, as a streaming body.
fn stream_then<S>(first: String, rest: S) -> Response
where
    S: futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    use futures_util::StreamExt;
    let head = futures_util::stream::once(async move { Ok(Bytes::from(format!("data: {first}\n\n"))) });
    ([(header::CONTENT_TYPE, "text/event-stream")], Body::from_stream(head.chain(rest))).into_response()
}

/// Counts itself in `dropped` when the stream it lives in is dropped early.
struct OnDrop(Arc<AtomicUsize>);

impl Drop for OnDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

async fn answer(State(sh): State<Shared>, uri: axum::http::Uri, headers: HeaderMap, body: Bytes) -> Response {
    let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    sh.seen.lock().unwrap().push(Seen { path: uri.path().to_string(), headers, body: json.clone() });
    let wants_usage = json["stream_options"]["include_usage"] == json!(true);
    match json["model"].as_str().unwrap_or("") {
        "json" => ([(header::CONTENT_TYPE, "application/json")], CHAT_JSON).into_response(),
        "embed" => axum::Json(json!({"object": "list", "data": [{"embedding": [0.1, 0.2]}], "usage": {"prompt_tokens": 3, "total_tokens": 3}})).into_response(),
        "error" => (StatusCode::BAD_REQUEST, [(header::CONTENT_TYPE, "application/json")], ERROR_JSON).into_response(),
        "speech" => ([(header::CONTENT_TYPE, "audio/mpeg")], vec![0u8, 1, 2, 255]).into_response(),
        "stream" => {
            let mut ev = vec![chunk("Hel"), chunk("lo")];
            if wants_usage {
                ev.push(usage_only());
            }
            ev.push("[DONE]".into());
            sse(ev)
        }
        "inline-usage" => {
            let last = json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 9, "completion_tokens": 1}});
            sse(vec![chunk("x"), last.to_string(), "[DONE]".into()])
        }
        "slow-headers" => {
            tokio::time::sleep(Duration::from_secs(3)).await;
            sse(vec!["[DONE]".into()])
        }
        "stall" => stream_then(
            chunk("a"),
            futures_util::stream::once(async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                Ok(Bytes::from_static(b"data: [DONE]\n\n"))
            }),
        ),
        "die" => stream_then(
            chunk("a"),
            // A pause first: failing in the same poll as the first chunk makes
            // hyper abort before the headers leave, which is a different case.
            futures_util::stream::once(async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Err(std::io::Error::other("upstream crashed"))
            }),
        ),
        "endless" => {
            let guard = OnDrop(sh.dropped.clone());
            let s = futures_util::stream::unfold(guard, |g| async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                Some((Ok(Bytes::from(format!("data: {}\n\n", chunk("tick")))), g))
            });
            stream_then(chunk("start"), s)
        }
        "resp-json" => ([(header::CONTENT_TYPE, "application/json")], RESP_JSON).into_response(),
        "resp-stream" => ([(header::CONTENT_TYPE, "text/event-stream")], RESP_STREAM).into_response(),
        "tools-stream" => {
            let call = |i: u64, id: Option<&str>, name: Option<&str>, args: &str| {
                let mut c = json!({"index": i, "function": {"arguments": args}});
                if let (Some(id), Some(name)) = (id, name) {
                    c["id"] = json!(id);
                    c["type"] = json!("function");
                    c["function"]["name"] = json!(name);
                }
                delta(json!({"tool_calls": [c]}))
            };
            let mut ev = vec![
                call(0, Some("call_a"), Some("shell"), ""),
                call(0, None, None, "{\"cmd\":"),
                call(0, None, None, "\"ls\"}"),
                call(1, Some("call_b"), Some("read"), "{}"),
                finish("tool_calls"),
            ];
            if wants_usage {
                ev.push(usage_only());
            }
            ev.push("[DONE]".into());
            sse(ev)
        }
        "think-stream" => {
            let mut ev = vec![
                delta(json!({"reasoning_content": "Hmm."})),
                delta(json!({"content": "Yes"})),
                finish("stop"),
            ];
            if wants_usage {
                ev.push(usage_only());
            }
            ev.push("[DONE]".into());
            sse(ev)
        }
        "think-json" => axum::Json(json!({
            "object": "chat.completion",
            "choices": [{"index": 0, "finish_reason": "stop", "message": {"role": "assistant", "content": "Yes", "reasoning_content": "Hmm."}}],
            "usage": {"prompt_tokens": 4, "completion_tokens": 2}
        }))
        .into_response(),
        "claude-json" => axum::Json(json!({
            "id": "msg_c1",
            "type": "message",
            "role": "assistant",
            "model": "claude-x",
            "content": [
                {"type": "thinking", "thinking": "Easy.", "signature": "sig"},
                {"type": "text", "text": "hi from claude"}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 4, "cache_read_input_tokens": 90, "cache_creation_input_tokens": 6, "output_tokens": 3}
        }))
        .into_response(),
        "claude-stream" => {
            let ev = |kind: &str, v: Value| format!("event: {kind}\ndata: {v}\n\n");
            let body = [
                ev("message_start", json!({"type": "message_start", "message": {"id": "msg_c2", "usage": {"input_tokens": 7, "output_tokens": 1}}})),
                ev("ping", json!({"type": "ping"})),
                ev("content_block_start", json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})),
                ev("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hel"}})),
                ev("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "lo"}})),
                ev("content_block_stop", json!({"type": "content_block_stop", "index": 0})),
                ev("content_block_start", json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "shell", "input": {}}})),
                ev("content_block_delta", json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"cmd\":"}})),
                ev("content_block_delta", json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "\"ls\"}"}})),
                ev("content_block_stop", json!({"type": "content_block_stop", "index": 1})),
                ev("message_delta", json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 9}})),
                ev("message_stop", json!({"type": "message_stop"})),
            ]
            .concat();
            ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
        }
        "claude-error" => (
            StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: too large"}}"#,
        )
            .into_response(),
        "claude-busy" => (
            StatusCode::from_u16(529).unwrap(),
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        )
            .into_response(),
        // The bench: one event at once, like a token stream's first token.
        "bench" => sse(vec![chunk("t"), "[DONE]".into()]),
        other => (StatusCode::NOT_FOUND, format!("no behaviour for {other}")).into_response(),
    }
}
