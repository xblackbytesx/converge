//! Anthropic's Messages API, translated from and to chat completions
//! (AGENTS.md, M5 plan). Chat completions stays the internal shape, so
//! Responses on an Anthropic connection is `responses` then this. Pure:
//! ids and times are passed in.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

/// The API version sent on every request. On it `temperature`, `top_p` and
/// `top_k` are refused, so they are always dropped.
pub const VERSION: &str = "2026-10-21";

/// Output token limits when neither the client nor a setting gave one.
pub const DEFAULT_MAX_TOKENS: i64 = 16_000;
pub const DEFAULT_MAX_TOKENS_STREAM: i64 = 64_000;

/// A chat request as a Messages request.
#[derive(Debug, Clone, PartialEq)]
pub struct Translated {
    pub body: Value,
    /// Chat fields with no Messages equivalent, accepted and not used.
    pub dropped: Vec<String>,
}

/// Per model: ask for summarized thinking (the model's reasoning switch)
/// and mark the request for prompt caching (its cache switch).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opts {
    pub thinking: bool,
    pub cache: bool,
}

const USED: &[&str] = &[
    "model",
    "messages",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "max_tokens",
    "max_completion_tokens",
    "stop",
    "reasoning_effort",
    "response_format",
    "user",
    "stream",
    // Converge's own (`openai::prepare`); usage always comes back.
    "stream_options",
    "n",
];

fn unsupported(what: &str) -> String {
    format!("{what} is not supported on Anthropic connections")
}

fn text_of(c: Option<&Value>) -> String {
    match c {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => {
            parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n")
        }
        _ => String::new(),
    }
}

/// An `image_url` part as an image block: data URLs inline, others by URL.
fn image(url: &str) -> Result<Value, String> {
    if let Some(rest) = url.strip_prefix("data:") {
        let (media, data) = rest.split_once(";base64,").ok_or("image data URLs must be base64")?;
        return Ok(json!({"type": "image", "source": {"type": "base64", "media_type": media, "data": data}}));
    }
    if url.starts_with("https://") || url.starts_with("http://") {
        return Ok(json!({"type": "image", "source": {"type": "url", "url": url}}));
    }
    Err("an image must be a data URL or an http(s) URL".into())
}

fn text_block(t: &str) -> Value {
    json!({"type": "text", "text": t})
}

fn user_blocks(c: Option<&Value>) -> Result<Vec<Value>, String> {
    match c {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::String(s)) => Ok(if s.is_empty() { vec![] } else { vec![text_block(s)] }),
        Some(Value::Array(parts)) => {
            let mut out = Vec::new();
            for p in parts {
                match p.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = p.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                            out.push(text_block(t));
                        }
                    }
                    Some("image_url") => {
                        let url = p["image_url"].get("url").and_then(Value::as_str).unwrap_or("");
                        out.push(image(url)?);
                    }
                    Some(t) => return Err(unsupported(&format!("`{t}` content"))),
                    None => return Err("every content part needs a `type`".into()),
                }
            }
            Ok(out)
        }
        Some(_) => Err("message `content` must be a string or a list of parts".into()),
    }
}

/// Appends to the last message when it has the same role: the Messages API
/// wants turns to alternate, and all tool results in one user turn.
fn push(out: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if let Some(last) = out.last_mut().filter(|m| m["role"] == role) {
        last["content"].as_array_mut().expect("content is a list").extend(blocks);
        return;
    }
    out.push(json!({"role": role, "content": blocks}));
}

/// Turns a chat completions request into a Messages request, or says what
/// cannot be translated.
pub fn to_messages(chat: &Value, opts: Opts) -> Result<Translated, String> {
    let obj = chat.as_object().ok_or("the request body must be a JSON object")?;
    if obj.get("n").and_then(Value::as_i64).is_some_and(|n| n > 1) {
        return Err(unsupported("`n` above 1"));
    }
    let mut dropped: Vec<String> =
        obj.iter().filter(|(k, v)| !v.is_null() && !USED.contains(&k.as_str())).map(|(k, _)| k.clone()).collect();
    let mut body = Map::new();
    if let Some(m) = obj.get("model") {
        body.insert("model".into(), m.clone());
    }

    let mut system: Vec<String> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();
    for m in obj.get("messages").and_then(Value::as_array).ok_or("the request needs `messages`")? {
        match m.get("role").and_then(Value::as_str).unwrap_or("") {
            "system" | "developer" => {
                let t = text_of(m.get("content"));
                if !t.trim().is_empty() {
                    system.push(t);
                }
            }
            "user" => push(&mut messages, "user", user_blocks(m.get("content"))?),
            "assistant" => {
                let mut blocks = Vec::new();
                let t = text_of(m.get("content"));
                if !t.is_empty() {
                    blocks.push(text_block(&t));
                }
                for c in m.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
                    let f = &c["function"];
                    let args = f.get("arguments").and_then(Value::as_str).unwrap_or("");
                    let input: Value = if args.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(args).map_err(|_| "a tool call's `arguments` is not valid JSON")?
                    };
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": c.get("id").and_then(Value::as_str).unwrap_or(""),
                        "name": f.get("name").and_then(Value::as_str).unwrap_or(""),
                        "input": input,
                    }));
                }
                push(&mut messages, "assistant", blocks);
            }
            "tool" => {
                let id =
                    m.get("tool_call_id").and_then(Value::as_str).ok_or("a `tool` message needs a `tool_call_id`")?;
                let block = json!({"type": "tool_result", "tool_use_id": id, "content": text_of(m.get("content"))});
                push(&mut messages, "user", vec![block]);
            }
            other => return Err(unsupported(&format!("the `{other}` role"))),
        }
    }
    if !system.is_empty() {
        body.insert("system".into(), Value::String(system.join("\n\n")));
    }
    body.insert("messages".into(), Value::Array(messages));

    let tools = obj.get("tools").and_then(Value::as_array).filter(|t| !t.is_empty());
    if let Some(tools) = tools {
        let mut out = Vec::new();
        for t in tools {
            if t.get("type").and_then(Value::as_str) != Some("function") {
                return Err(unsupported("a tool that is not a function"));
            }
            let f = &t["function"];
            let mut tool = Map::new();
            tool.insert("name".into(), f.get("name").cloned().unwrap_or(Value::Null));
            if let Some(d) = f.get("description").filter(|d| d.is_string()) {
                tool.insert("description".into(), d.clone());
            }
            let schema = f.get("parameters").filter(|p| p.is_object()).cloned();
            tool.insert("input_schema".into(), schema.unwrap_or_else(|| json!({"type": "object", "properties": {}})));
            if let Some(s) = f.get("strict").filter(|s| s.is_boolean()) {
                tool.insert("strict".into(), s.clone());
            }
            out.push(Value::Object(tool));
        }
        body.insert("tools".into(), Value::Array(out));

        let mut choice = match obj.get("tool_choice") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(match s.as_str() {
                "auto" => json!({"type": "auto"}),
                "none" => json!({"type": "none"}),
                "required" => json!({"type": "any"}),
                other => return Err(format!("unknown `tool_choice` `{other}`")),
            }),
            Some(v) => {
                let name = v.pointer("/function/name").and_then(Value::as_str).ok_or("unknown `tool_choice`")?;
                Some(json!({"type": "tool", "name": name}))
            }
        };
        if obj.get("parallel_tool_calls") == Some(&Value::Bool(false)) {
            let c = choice.get_or_insert_with(|| json!({"type": "auto"}));
            if c["type"] != "none" {
                c["disable_parallel_tool_use"] = Value::Bool(true);
            }
        }
        if let Some(c) = choice {
            body.insert("tool_choice".into(), c);
        }
    }

    let stream = obj.get("stream") == Some(&Value::Bool(true));
    let limit = ["max_tokens", "max_completion_tokens"].iter().find_map(|k| obj.get(*k).and_then(Value::as_i64));
    let fallback = if stream { DEFAULT_MAX_TOKENS_STREAM } else { DEFAULT_MAX_TOKENS };
    body.insert("max_tokens".into(), Value::from(limit.unwrap_or(fallback)));
    match obj.get("stop") {
        Some(Value::String(s)) => {
            body.insert("stop_sequences".into(), json!([s]));
        }
        Some(a @ Value::Array(_)) => {
            body.insert("stop_sequences".into(), a.clone());
        }
        _ => {}
    }

    let mut output = Map::new();
    if let Some(e) = obj.get("reasoning_effort").and_then(Value::as_str) {
        let e = if matches!(e, "none" | "minimal") { "low" } else { e };
        output.insert("effort".into(), Value::from(e));
    }
    match obj.get("response_format").and_then(|f| f.get("type")).and_then(Value::as_str) {
        Some("json_schema") => {
            let schema = obj["response_format"].pointer("/json_schema/schema").cloned();
            let schema = schema.ok_or("`response_format.json_schema` needs a `schema`")?;
            output.insert("format".into(), json!({"type": "json_schema", "schema": schema}));
        }
        Some("json_object") => dropped.push("response_format".into()),
        _ => {}
    }
    if !output.is_empty() {
        body.insert("output_config".into(), Value::Object(output));
    }
    if let Some(u) = obj.get("user").and_then(Value::as_str) {
        body.insert("metadata".into(), json!({"user_id": u}));
    }
    if opts.thinking {
        body.insert("thinking".into(), json!({"type": "adaptive", "display": "summarized"}));
    }
    if opts.cache {
        body.insert("cache_control".into(), json!({"type": "ephemeral"}));
    }
    if stream {
        body.insert("stream".into(), Value::Bool(true));
    }
    dropped.sort();
    Ok(Translated { body: Value::Object(body), dropped })
}

// ------------------------------------------------------------- answers

fn finish_reason(stop: Option<&str>) -> &'static str {
    match stop {
        Some("max_tokens" | "model_context_window_exceeded") => "length",
        Some("tool_use") => "tool_calls",
        Some("refusal") => "content_filter",
        _ => "stop",
    }
}

/// Messages usage in chat terms: prompt tokens include cache reads and
/// writes, `cached_tokens` the reads.
fn usage_out(u: &Map<String, Value>) -> Value {
    let n = |k: &str| u.get(k).and_then(Value::as_i64).unwrap_or(0);
    let read = n("cache_read_input_tokens");
    let prompt = n("input_tokens") + read + n("cache_creation_input_tokens");
    let completion = n("output_tokens");
    json!({
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "total_tokens": prompt + completion,
        "prompt_tokens_details": {"cached_tokens": read},
    })
}

/// A Messages answer as a chat completion. `model` is the id the client
/// asked for.
pub fn from_message(msg: &Value, model: &str, created: i64) -> Value {
    let (mut text, mut thinking, mut calls) = (String::new(), String::new(), Vec::new());
    for b in msg.get("content").and_then(Value::as_array).into_iter().flatten() {
        match b.get("type").and_then(Value::as_str) {
            Some("text") => text.push_str(b.get("text").and_then(Value::as_str).unwrap_or("")),
            Some("thinking") => thinking.push_str(b.get("thinking").and_then(Value::as_str).unwrap_or("")),
            Some("tool_use") => calls.push(json!({
                "id": b["id"],
                "type": "function",
                "function": {"name": b["name"], "arguments": b.get("input").cloned().unwrap_or(json!({})).to_string()},
            })),
            _ => {}
        }
    }
    let mut message = Map::new();
    message.insert("role".into(), Value::from("assistant"));
    let content = if text.is_empty() && !calls.is_empty() { Value::Null } else { Value::String(text) };
    message.insert("content".into(), content);
    if !thinking.is_empty() {
        message.insert("reasoning_content".into(), Value::String(thinking));
    }
    if !calls.is_empty() {
        message.insert("tool_calls".into(), Value::Array(calls));
    }
    let stop = msg.get("stop_reason").and_then(Value::as_str);
    let mut out = json!({
        "id": msg.get("id").cloned().unwrap_or(Value::Null),
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": finish_reason(stop)}],
    });
    if let Some(u) = msg.get("usage").and_then(Value::as_object) {
        out["usage"] = usage_out(u);
    }
    out
}

/// An Anthropic error answer in OpenAI's error shape. `529 overloaded`
/// becomes `503`, which OpenAI clients retry.
pub fn error_to_openai(status: u16, body: &[u8]) -> (u16, Value) {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let kind = v.pointer("/error/type").and_then(Value::as_str).unwrap_or("api_error");
    let message = v.pointer("/error/message").and_then(Value::as_str).map(str::to_string);
    let message = message.unwrap_or_else(|| format!("the upstream answered {status}"));
    let openai_kind = match kind {
        "invalid_request_error"
        | "authentication_error"
        | "permission_error"
        | "not_found_error"
        | "rate_limit_error" => kind,
        _ => "server_error",
    };
    let status = if status == 529 { 503 } else { status };
    (status, json!({"error": {"message": message, "type": openai_kind, "param": null, "code": kind}}))
}

/// What a content block index is, for its deltas.
enum Block {
    Text,
    Thinking,
    /// The chat tool call index it became.
    Tool(u64),
    Other,
}

/// Turns a Messages stream into chat completion chunks, event by event.
pub struct Stream {
    id: String,
    model: String,
    created: i64,
    blocks: HashMap<u64, Block>,
    tools: u64,
    usage: Map<String, Value>,
    finish: Option<&'static str>,
    ended: bool,
}

impl Stream {
    pub fn new(model: &str, created: i64) -> Self {
        Stream {
            id: String::new(),
            model: model.to_string(),
            created,
            blocks: HashMap::new(),
            tools: 0,
            usage: Map::new(),
            finish: None,
            ended: false,
        }
    }

    /// The stream reached `message_stop` (or an error event): a stream
    /// that ends without it was cut.
    pub fn ended(&self) -> bool {
        self.ended
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        })
    }

    fn take_usage(&mut self, u: Option<&Value>) {
        for (k, v) in u.and_then(Value::as_object).into_iter().flatten() {
            if v.is_number() {
                self.usage.insert(k.clone(), v.clone());
            }
        }
    }

    /// One event (already parsed), giving zero or more chat chunks.
    pub fn push(&mut self, ev: &Value) -> Vec<Value> {
        if self.ended {
            return vec![];
        }
        let index = ev.get("index").and_then(Value::as_u64).unwrap_or(0);
        match ev.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                let m = &ev["message"];
                self.id = m.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                self.take_usage(m.get("usage"));
                vec![self.chunk(json!({"role": "assistant", "content": ""}), None)]
            }
            Some("content_block_start") => {
                let b = &ev["content_block"];
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        self.blocks.insert(index, Block::Text);
                        match b.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                            Some(t) => vec![self.chunk(json!({"content": t}), None)],
                            None => vec![],
                        }
                    }
                    Some("thinking") => {
                        self.blocks.insert(index, Block::Thinking);
                        vec![]
                    }
                    Some("tool_use") => {
                        let n = self.tools;
                        self.tools += 1;
                        self.blocks.insert(index, Block::Tool(n));
                        let call = json!({"index": n, "id": b["id"], "type": "function", "function": {"name": b["name"], "arguments": ""}});
                        vec![self.chunk(json!({"tool_calls": [call]}), None)]
                    }
                    _ => {
                        self.blocks.insert(index, Block::Other);
                        vec![]
                    }
                }
            }
            Some("content_block_delta") => {
                let d = &ev["delta"];
                let s = |k: &str| d.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                let delta = match (self.blocks.get(&index), d.get("type").and_then(Value::as_str)) {
                    (Some(Block::Text), Some("text_delta")) => json!({"content": s("text")}),
                    (Some(Block::Thinking), Some("thinking_delta")) => json!({"reasoning_content": s("thinking")}),
                    (Some(Block::Tool(n)), Some("input_json_delta")) => {
                        json!({"tool_calls": [{"index": n, "function": {"arguments": s("partial_json")}}]})
                    }
                    _ => return vec![],
                };
                vec![self.chunk(delta, None)]
            }
            Some("message_delta") => {
                self.finish = Some(finish_reason(ev.pointer("/delta/stop_reason").and_then(Value::as_str)));
                self.take_usage(ev.get("usage"));
                vec![]
            }
            Some("message_stop") => {
                self.ended = true;
                let done = self.chunk(json!({}), Some(self.finish.unwrap_or("stop")));
                let mut usage = self.chunk(json!({}), None);
                usage["choices"] = json!([]);
                usage["usage"] = usage_out(&self.usage);
                vec![done, usage]
            }
            Some("error") => {
                self.ended = true;
                let kind = ev.pointer("/error/type").and_then(Value::as_str).unwrap_or("api_error");
                let message = ev.pointer("/error/message").and_then(Value::as_str).unwrap_or("the upstream failed");
                vec![json!({"error": {"message": message, "type": "server_error", "code": kind}})]
            }
            // `ping`, `content_block_stop`, anything newer.
            _ => vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ON: Opts = Opts { thinking: false, cache: false };

    fn msgs(chat: Value) -> Value {
        to_messages(&chat, ON).unwrap().body
    }

    #[test]
    fn a_simple_request_and_the_fields_that_map() {
        let t = to_messages(
            &json!({
                "model": "claude",
                "messages": [
                    {"role": "system", "content": "Persona.\n\nClient."},
                    {"role": "user", "content": "hi"},
                    {"role": "developer", "content": "Later rules."}
                ],
                "temperature": 0.2,
                "top_p": 0.9,
                "seed": 1,
                "max_completion_tokens": 500,
                "stop": "END",
                "reasoning_effort": "minimal",
                "user": "kim",
                "stream": true,
                "stream_options": {"include_usage": true},
                "response_format": {"type": "json_schema", "json_schema": {"name": "x", "schema": {"type": "object"}}}
            }),
            Opts { thinking: true, cache: true },
        )
        .unwrap();
        assert_eq!(
            t.body,
            json!({
                "model": "claude",
                "system": "Persona.\n\nClient.\n\nLater rules.",
                "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
                "max_tokens": 500,
                "stop_sequences": ["END"],
                "output_config": {"effort": "low", "format": {"type": "json_schema", "schema": {"type": "object"}}},
                "metadata": {"user_id": "kim"},
                "thinking": {"type": "adaptive", "display": "summarized"},
                "cache_control": {"type": "ephemeral"},
                "stream": true
            })
        );
        assert_eq!(t.dropped, ["seed", "temperature", "top_p"]);
        let j = to_messages(
            &json!({"messages": [{"role": "user", "content": "x"}], "response_format": {"type": "json_object"}}),
            ON,
        )
        .unwrap();
        assert_eq!(j.dropped, ["response_format"]);
    }

    #[test]
    fn token_limit_fallbacks() {
        let m = |extra: Value| {
            let mut c = json!({"messages": [{"role": "user", "content": "x"}]});
            c.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            msgs(c)["max_tokens"].clone()
        };
        assert_eq!(m(json!({})), DEFAULT_MAX_TOKENS);
        assert_eq!(m(json!({"stream": true})), DEFAULT_MAX_TOKENS_STREAM);
        assert_eq!(m(json!({"max_tokens": 77})), 77);
    }

    #[test]
    fn a_tool_loop_and_images_become_blocks() {
        let b = msgs(json!({"messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "Look"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
                {"type": "image_url", "image_url": {"url": "https://x.example/a.jpg"}}
            ]},
            {"role": "assistant", "content": "Checking.", "tool_calls": [
                {"id": "t1", "type": "function", "function": {"name": "shell", "arguments": "{\"cmd\":\"ls\"}"}},
                {"id": "t2", "type": "function", "function": {"name": "read", "arguments": ""}}
            ]},
            {"role": "tool", "tool_call_id": "t1", "content": "a.txt"},
            {"role": "tool", "tool_call_id": "t2", "content": [{"type": "text", "text": "x"}]},
            {"role": "user", "content": "and now?"}
        ]}));
        assert_eq!(
            b["messages"],
            json!([
                {"role": "user", "content": [
                    {"type": "text", "text": "Look"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
                    {"type": "image", "source": {"type": "url", "url": "https://x.example/a.jpg"}}
                ]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Checking."},
                    {"type": "tool_use", "id": "t1", "name": "shell", "input": {"cmd": "ls"}},
                    {"type": "tool_use", "id": "t2", "name": "read", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "a.txt"},
                    {"type": "tool_result", "tool_use_id": "t2", "content": "x"},
                    {"type": "text", "text": "and now?"}
                ]}
            ])
        );
    }

    #[test]
    fn tools_and_tool_choice() {
        let base = |choice: Value, parallel: Value| {
            msgs(json!({
                "messages": [{"role": "user", "content": "x"}],
                "tools": [{"type": "function", "function": {"name": "shell", "description": "Run", "parameters": {"type": "object"}, "strict": true}}, {"type": "function", "function": {"name": "now"}}],
                "tool_choice": choice,
                "parallel_tool_calls": parallel
            }))
        };
        let b = base(json!("required"), json!(false));
        assert_eq!(
            b["tools"],
            json!([
                {"name": "shell", "description": "Run", "input_schema": {"type": "object"}, "strict": true},
                {"name": "now", "input_schema": {"type": "object", "properties": {}}}
            ])
        );
        assert_eq!(b["tool_choice"], json!({"type": "any", "disable_parallel_tool_use": true}));
        assert_eq!(
            base(json!({"type": "function", "function": {"name": "now"}}), Value::Null)["tool_choice"],
            json!({"type": "tool", "name": "now"})
        );
        assert_eq!(
            base(Value::Null, json!(false))["tool_choice"],
            json!({"type": "auto", "disable_parallel_tool_use": true})
        );
        assert_eq!(base(json!("none"), json!(false))["tool_choice"], json!({"type": "none"}));
        assert!(base(Value::Null, Value::Null).get("tool_choice").is_none());
    }

    #[test]
    fn what_cannot_be_translated_is_refused() {
        for (chat, says) in [
            (json!({"messages": [{"role": "user", "content": "x"}], "n": 2}), "`n`"),
            (
                json!({"messages": [{"role": "assistant", "tool_calls": [{"id": "a", "function": {"name": "f", "arguments": "{oops"}}]}]}),
                "not valid JSON",
            ),
            (json!({"messages": [{"role": "user", "content": [{"type": "input_audio"}]}]}), "`input_audio`"),
            (
                json!({"messages": [{"role": "user", "content": [{"type": "image_url", "image_url": {"url": "file:///x"}}]}]}),
                "data URL",
            ),
            (json!({"messages": [{"role": "function", "content": "x"}]}), "`function` role"),
            (json!({"messages": [{"role": "tool", "content": "x"}]}), "tool_call_id"),
            (json!({"model": "x"}), "`messages`"),
        ] {
            let e = to_messages(&chat, ON).unwrap_err();
            assert!(e.contains(says), "{chat}: {e}");
        }
    }

    #[test]
    fn an_answer_becomes_a_chat_completion() {
        let msg = json!({
            "id": "msg_1",
            "type": "message",
            "model": "claude-opus-5-5",
            "content": [
                {"type": "thinking", "thinking": "They want ls.", "signature": "sig"},
                {"type": "text", "text": "On it."},
                {"type": "tool_use", "id": "toolu_1", "name": "shell", "input": {"cmd": "ls"}},
                {"type": "fallback", "from": {}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "cache_read_input_tokens": 1000, "cache_creation_input_tokens": 50, "output_tokens": 20}
        });
        assert_eq!(
            from_message(&msg, "claude", 1700000000),
            json!({
                "id": "msg_1",
                "object": "chat.completion",
                "created": 1700000000,
                "model": "claude",
                "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
                    "role": "assistant",
                    "content": "On it.",
                    "reasoning_content": "They want ls.",
                    "tool_calls": [{"id": "toolu_1", "type": "function", "function": {"name": "shell", "arguments": "{\"cmd\":\"ls\"}"}}]
                }}],
                "usage": {"prompt_tokens": 1060, "completion_tokens": 20, "total_tokens": 1080, "prompt_tokens_details": {"cached_tokens": 1000}}
            })
        );
        for (stop, finish) in [
            ("end_turn", "stop"),
            ("stop_sequence", "stop"),
            ("max_tokens", "length"),
            ("model_context_window_exceeded", "length"),
            ("refusal", "content_filter"),
        ] {
            let m = json!({"content": [], "stop_reason": stop});
            assert_eq!(from_message(&m, "c", 0)["choices"][0]["finish_reason"], finish, "{stop}");
        }
        let only_tool =
            json!({"content": [{"type": "tool_use", "id": "a", "name": "f", "input": {}}], "stop_reason": "tool_use"});
        assert_eq!(from_message(&only_tool, "c", 0)["choices"][0]["message"]["content"], Value::Null);
    }

    #[test]
    fn errors_get_openais_shape() {
        let (s, v) =
            error_to_openai(400, br#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#);
        assert_eq!(
            (s, v),
            (
                400,
                json!({"error": {"message": "bad", "type": "invalid_request_error", "param": null, "code": "invalid_request_error"}})
            )
        );
        let (s, v) =
            error_to_openai(529, br#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#);
        assert_eq!(s, 503);
        assert_eq!(v["error"]["type"], "server_error");
        let (s, v) = error_to_openai(502, b"<html>");
        assert_eq!((s, v["error"]["message"].as_str().unwrap()), (502, "the upstream answered 502"));
    }

    fn run(events: &[Value]) -> (Vec<Value>, bool) {
        let mut s = Stream::new("claude", 7);
        let out: Vec<Value> = events.iter().flat_map(|e| s.push(e)).collect();
        (out, s.ended())
    }

    fn delta(index: u64, d: Value) -> Value {
        json!({"type": "content_block_delta", "index": index, "delta": d})
    }

    #[test]
    fn a_stream_of_thinking_text_and_two_tool_calls() {
        let (out, ended) = run(&[
            json!({"type": "message_start", "message": {"id": "msg_9", "usage": {"input_tokens": 5, "cache_read_input_tokens": 100, "output_tokens": 1}}}),
            json!({"type": "ping"}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
            delta(0, json!({"type": "thinking_delta", "thinking": "Hmm."})),
            delta(0, json!({"type": "signature_delta", "signature": "abc"})),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
            delta(1, json!({"type": "text_delta", "text": "Hi"})),
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "t1", "name": "shell", "input": {}}}),
            delta(2, json!({"type": "input_json_delta", "partial_json": "{\"cmd\":"})),
            delta(2, json!({"type": "input_json_delta", "partial_json": "\"ls\"}"})),
            json!({"type": "content_block_start", "index": 3, "content_block": {"type": "tool_use", "id": "t2", "name": "read", "input": {}}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 30}}),
            json!({"type": "message_stop"}),
        ]);
        assert!(ended);
        let deltas: Vec<Value> = out.iter().map(|c| c["choices"][0]["delta"].clone()).collect();
        assert_eq!(
            deltas,
            [
                json!({"role": "assistant", "content": ""}),
                json!({"reasoning_content": "Hmm."}),
                json!({"content": "Hi"}),
                json!({"tool_calls": [{"index": 0, "id": "t1", "type": "function", "function": {"name": "shell", "arguments": ""}}]}),
                json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"cmd\":"}}]}),
                json!({"tool_calls": [{"index": 0, "function": {"arguments": "\"ls\"}"}}]}),
                json!({"tool_calls": [{"index": 1, "id": "t2", "type": "function", "function": {"name": "read", "arguments": ""}}]}),
                json!({}),
                Value::Null,
            ]
        );
        assert_eq!(out[0]["id"], "msg_9");
        assert_eq!(out[0]["model"], "claude");
        assert_eq!(out[7]["choices"][0]["finish_reason"], "tool_calls");
        let last = out.last().unwrap();
        assert!(crate::openai::is_usage_only_chunk(last), "the usage chunk the include_usage rule knows");
        assert_eq!(
            last["usage"],
            json!({"prompt_tokens": 105, "completion_tokens": 30, "total_tokens": 135, "prompt_tokens_details": {"cached_tokens": 100}})
        );
    }

    #[test]
    fn errors_end_the_stream_and_a_cut_stream_is_not_ended() {
        let (out, ended) = run(&[
            json!({"type": "message_start", "message": {"id": "m", "usage": {}}}),
            json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
            json!({"type": "message_stop"}),
        ]);
        assert!(ended);
        assert_eq!(out.len(), 2, "nothing after the error");
        assert_eq!(
            out[1],
            json!({"error": {"message": "Overloaded", "type": "server_error", "code": "overloaded_error"}})
        );
        let (_, ended) = run(&[json!({"type": "message_start", "message": {"id": "m"}})]);
        assert!(!ended);
    }
}
