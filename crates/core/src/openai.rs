//! The OpenAI wire format, only as far as the proxy needs it: which endpoint,
//! rewriting `model`, asking for usage on streams, and reading usage back.
//! Everything else in a request or answer passes through untouched.

use serde_json::{Map, Value};

/// The model-routed endpoints, by the path after `/v1/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    Chat,
    Completions,
    Embeddings,
    Rerank,
    Images,
    Speech,
    /// The Responses API: native or translated over chat (`responses`).
    Responses,
}

impl Endpoint {
    pub fn from_path(path: &str) -> Option<Self> {
        Some(match path {
            "chat/completions" => Endpoint::Chat,
            "completions" => Endpoint::Completions,
            "embeddings" => Endpoint::Embeddings,
            "rerank" => Endpoint::Rerank,
            "images/generations" => Endpoint::Images,
            "audio/speech" => Endpoint::Speech,
            "responses" => Endpoint::Responses,
            _ => return None,
        })
    }

    pub fn path(self) -> &'static str {
        match self {
            Endpoint::Chat => "chat/completions",
            Endpoint::Completions => "completions",
            Endpoint::Embeddings => "embeddings",
            Endpoint::Rerank => "rerank",
            Endpoint::Images => "images/generations",
            Endpoint::Speech => "audio/speech",
            Endpoint::Responses => "responses",
        }
    }

    /// Endpoints that can stream tokens and report usage on the stream.
    fn streams_usage(self) -> bool {
        matches!(self, Endpoint::Chat | Endpoint::Completions)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RequestError {
    #[error("the request body must be a JSON object")]
    NotAnObject,
    #[error("the request needs a \"model\"")]
    NoModel,
}

/// The model a request names. The body is parsed once, by the caller.
pub fn model(body: &Value) -> Result<&str, RequestError> {
    let obj = body.as_object().ok_or(RequestError::NotAnObject)?;
    obj.get("model").and_then(Value::as_str).filter(|m| !m.is_empty()).ok_or(RequestError::NoModel)
}

/// What [`prepare`] did to a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prepared {
    pub stream: bool,
    /// Converge added `stream_options.include_usage`; the usage-only chunk
    /// that brings must be removed before the client sees it.
    pub injected_usage: bool,
}

/// Points the request at the upstream's own model name and, for a stream
/// that did not ask for usage, asks for it. Nothing else changes.
pub fn prepare(body: &mut Value, endpoint: Endpoint, upstream_model: &str) -> Prepared {
    let Some(obj) = body.as_object_mut() else { return Prepared { stream: false, injected_usage: false } };
    obj.insert("model".into(), Value::String(upstream_model.to_string()));
    let stream = obj.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let mut injected_usage = false;
    if stream && endpoint.streams_usage() {
        let opts = obj.entry("stream_options").or_insert_with(|| Value::Object(Map::new()));
        if !opts.is_object() {
            *opts = Value::Object(Map::new());
        }
        let opts = opts.as_object_mut().expect("just made an object");
        if opts.get("include_usage").and_then(Value::as_bool) != Some(true) {
            opts.insert("include_usage".into(), Value::Bool(true));
            injected_usage = true;
        }
    }
    Prepared { stream, injected_usage }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cached_tokens: i64,
}

/// The `usage` object of an answer or stream chunk, when it has one. Accepts
/// both naming styles (`prompt_tokens` and the newer `input_tokens`), since
/// embeddings and some upstreams use the latter.
pub fn usage(v: &Value) -> Option<Usage> {
    let u = v.get("usage")?.as_object()?;
    let n = |keys: &[&str]| keys.iter().find_map(|k| u.get(*k).and_then(Value::as_i64)).unwrap_or(0);
    let cached = ["prompt_tokens_details", "input_tokens_details"]
        .iter()
        .find_map(|k| u.get(*k).and_then(|d| d.get("cached_tokens")).and_then(Value::as_i64))
        .unwrap_or(0);
    Some(Usage {
        prompt_tokens: n(&["prompt_tokens", "input_tokens"]),
        completion_tokens: n(&["completion_tokens", "output_tokens"]),
        cached_tokens: cached,
    })
}

/// Usage on a Responses stream: the final `response.completed` (or
/// `incomplete`/`failed`) event carries the whole response, usage inside.
pub fn stream_usage(v: &Value) -> Option<Usage> {
    usage(v).or_else(|| v.get("response").and_then(usage))
}

/// Reads one stream event's JSON. `[DONE]` and non-JSON data give None.
pub fn event_json(event: &[u8]) -> Option<Value> {
    let data = crate::sse::data(event)?;
    serde_json::from_str(&data).ok()
}

/// The chunk `include_usage` adds: usage and an empty `choices` list. Only
/// this exact shape is dropped; usage riding on a content chunk stays.
pub fn is_usage_only_chunk(v: &Value) -> bool {
    v.get("choices").and_then(Value::as_array).is_some_and(Vec::is_empty)
        && v.get("usage").is_some_and(Value::is_object)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn endpoints_round_trip() {
        for e in [
            Endpoint::Chat,
            Endpoint::Completions,
            Endpoint::Embeddings,
            Endpoint::Rerank,
            Endpoint::Images,
            Endpoint::Speech,
            Endpoint::Responses,
        ] {
            assert_eq!(Endpoint::from_path(e.path()), Some(e));
        }
        assert_eq!(Endpoint::from_path("files"), None);
    }

    #[test]
    fn model_is_required() {
        assert_eq!(model(&json!({"model": "local/qwen"})), Ok("local/qwen"));
        assert_eq!(model(&json!({"model": ""})), Err(RequestError::NoModel));
        assert_eq!(model(&json!({"model": 3})), Err(RequestError::NoModel));
        assert_eq!(model(&json!([1])), Err(RequestError::NotAnObject));
    }

    #[test]
    fn prepare_rewrites_model_and_keeps_the_rest() {
        let mut b =
            json!({"model": "local/qwen", "messages": [{"role": "user", "content": "hi"}], "x_vendor": {"k": 1}});
        let p = prepare(&mut b, Endpoint::Chat, "qwen3-30b");
        assert_eq!(p, Prepared { stream: false, injected_usage: false });
        assert_eq!(
            b,
            json!({"model": "qwen3-30b", "messages": [{"role": "user", "content": "hi"}], "x_vendor": {"k": 1}})
        );
    }

    #[test]
    fn prepare_asks_for_usage_only_when_missing() {
        let mut b = json!({"model": "m", "stream": true});
        assert!(prepare(&mut b, Endpoint::Chat, "u").injected_usage);
        assert_eq!(b["stream_options"], json!({"include_usage": true}));

        let mut b = json!({"model": "m", "stream": true, "stream_options": {"include_usage": true, "other": 1}});
        assert!(!prepare(&mut b, Endpoint::Completions, "u").injected_usage);
        assert_eq!(b["stream_options"], json!({"include_usage": true, "other": 1}));

        let mut b = json!({"model": "m", "stream": true, "stream_options": {"include_usage": false, "other": 1}});
        assert!(prepare(&mut b, Endpoint::Chat, "u").injected_usage);
        assert_eq!(b["stream_options"], json!({"include_usage": true, "other": 1}));

        let mut b = json!({"model": "m", "stream": true});
        assert!(!prepare(&mut b, Endpoint::Embeddings, "u").injected_usage, "only token streams");
        assert!(b.get("stream_options").is_none());
    }

    #[test]
    fn usage_in_both_styles() {
        let chat = json!({"usage": {"prompt_tokens": 10, "completion_tokens": 5, "prompt_tokens_details": {"cached_tokens": 4}}});
        assert_eq!(usage(&chat), Some(Usage { prompt_tokens: 10, completion_tokens: 5, cached_tokens: 4 }));
        let newer =
            json!({"usage": {"input_tokens": 7, "output_tokens": 2, "input_tokens_details": {"cached_tokens": 1}}});
        assert_eq!(usage(&newer), Some(Usage { prompt_tokens: 7, completion_tokens: 2, cached_tokens: 1 }));
        let emb = json!({"usage": {"prompt_tokens": 3, "total_tokens": 3}});
        assert_eq!(usage(&emb), Some(Usage { prompt_tokens: 3, ..Default::default() }));
        assert_eq!(usage(&json!({"usage": null})), None);
        assert_eq!(usage(&json!({})), None);
    }

    #[test]
    fn only_the_usage_only_chunk_is_droppable() {
        let only = event_json(b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1}}\n\n").unwrap();
        assert!(is_usage_only_chunk(&only));
        let riding = event_json(b"data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":1}}\n\n").unwrap();
        assert!(!is_usage_only_chunk(&riding));
        assert!(event_json(b"data: [DONE]\n\n").is_none());
    }
}
