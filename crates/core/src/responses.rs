//! The Responses API (AGENTS.md, M4 plan). Native connections get the
//! persona applied in Responses terms ([`apply_native_prompt`],
//! [`fill_native`]); translated connections get the request turned into
//! chat completions ([`to_chat`]) and the answer turned back
//! ([`from_chat`] for JSON, [`Stream`] for SSE). Pure: ids and times are
//! passed in.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use crate::persona::{Param, Params, PromptMode, pick};

// ---------------------------------------------------------------- native

/// The persona prompt in `instructions`: prepend puts it ahead of the
/// client's (blank line between); replace makes it the only one and drops
/// `system` and `developer` input messages.
pub fn apply_native_prompt(body: &mut Map<String, Value>, prompt: &str, mode: PromptMode) {
    let has_prompt = !prompt.trim().is_empty();
    let theirs = body.get("instructions").and_then(Value::as_str).filter(|s| !s.trim().is_empty()).map(str::to_string);
    match mode {
        PromptMode::Prepend if !has_prompt => {}
        PromptMode::Prepend => {
            let text = match theirs {
                Some(t) => format!("{prompt}\n\n{t}"),
                None => prompt.to_string(),
            };
            body.insert("instructions".into(), Value::String(text));
        }
        PromptMode::Replace => {
            if has_prompt {
                body.insert("instructions".into(), Value::String(prompt.to_string()));
            } else {
                body.remove("instructions");
            }
            if let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) {
                items.retain(|i| !matches!(i.get("role").and_then(Value::as_str), Some("system" | "developer")));
            }
        }
    }
}

/// The parameters native Responses has a field for, by the usual rule
/// ([`pick`]): `temperature`, `top_p`, `max_tokens` as `max_output_tokens`,
/// `reasoning_effort` as `reasoning.effort`. The penalties, `seed` and
/// `stop` have no Responses field and are skipped.
pub fn fill_native(body: &mut Map<String, Value>, persona: Option<&Params>, model: &Params) {
    let sent = |b: &Map<String, Value>, k: &str| b.get(k).is_some_and(|v| !v.is_null());
    for p in [Param::Temperature, Param::TopP] {
        if let Some(v) = pick(p, sent(body, p.key()), persona, model).cloned() {
            body.insert(p.key().into(), v);
        }
    }
    if let Some(v) = pick(Param::MaxTokens, sent(body, "max_output_tokens"), persona, model).cloned() {
        body.insert("max_output_tokens".into(), v);
    }
    let effort_sent = body.get("reasoning").and_then(|r| r.get("effort")).is_some_and(|v| !v.is_null());
    if let Some(v) = pick(Param::ReasoningEffort, effort_sent, persona, model).cloned() {
        let r = body.entry("reasoning").or_insert_with(|| Value::Object(Map::new()));
        if !r.is_object() {
            *r = Value::Object(Map::new());
        }
        r.as_object_mut().expect("just made an object").insert("effort".into(), v);
    }
}

// --------------------------------------------------------- request: to chat

/// A Responses request as chat completions.
#[derive(Debug, Clone, PartialEq)]
pub struct Translated {
    pub chat: Value,
    /// Request fields with no chat equivalent, accepted and not used. For
    /// the `converge-dropped-params` header.
    pub dropped: Vec<String>,
    /// Request fields a Response object repeats back.
    pub echo: Map<String, Value>,
}

/// Fields the translation reads (or refuses when set).
const USED: &[&str] = &[
    "model",
    "input",
    "instructions",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "temperature",
    "top_p",
    "max_output_tokens",
    "reasoning",
    "text",
    "stream",
    "previous_response_id",
    "conversation",
    "background",
    "prompt",
];

/// Repeated in the Response object, as OpenAI does.
const ECHO: &[&str] = &[
    "instructions",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "temperature",
    "top_p",
    "max_output_tokens",
    "reasoning",
    "text",
    "metadata",
];

fn native_only(what: &str) -> String {
    format!("{what} needs a connection with native Responses; this one translates to chat completions")
}

fn is_set(obj: &Map<String, Value>, k: &str) -> bool {
    obj.get(k).is_some_and(|v| !v.is_null())
}

/// Turns a Responses request into chat completions, or says which field
/// cannot be translated. `token_field` is the connection's declared name
/// for the output token limit.
pub fn to_chat(req: &Value, token_field: &str) -> Result<Translated, String> {
    let obj = req.as_object().ok_or("the request body must be a JSON object")?;
    for k in ["previous_response_id", "conversation"] {
        if is_set(obj, k) {
            return Err(format!(
                "`{k}` needs stored responses, and Converge stores none on this connection: \
                 send the whole conversation in `input` (with `store: false`)"
            ));
        }
    }
    if obj.get("background") == Some(&Value::Bool(true)) {
        return Err("`background` responses are not supported through Converge".into());
    }
    if is_set(obj, "prompt") {
        return Err("stored prompt templates (`prompt`) are not supported; send `instructions`".into());
    }

    let mut chat = Map::new();
    let mut dropped: Vec<String> =
        obj.iter().filter(|(k, v)| !v.is_null() && !USED.contains(&k.as_str())).map(|(k, _)| k.clone()).collect();
    let echo: Map<String, Value> =
        obj.iter().filter(|(k, _)| ECHO.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
    if let Some(m) = obj.get("model") {
        chat.insert("model".into(), m.clone());
    }

    let mut messages: Vec<Value> = Vec::new();
    if let Some(i) = obj.get("instructions").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
        messages.push(json!({"role": "system", "content": i}));
    }
    match obj.get("input") {
        Some(Value::String(s)) => messages.push(json!({"role": "user", "content": s})),
        Some(Value::Array(items)) => {
            for item in items {
                input_item(item, &mut messages)?;
            }
        }
        None | Some(Value::Null) => {}
        Some(_) => return Err("`input` must be a string or a list of items".into()),
    }
    if !messages.iter().any(|m| m["role"] != "system") {
        return Err("`input` is required".into());
    }
    chat.insert("messages".into(), Value::Array(messages));

    if let Some(tools) = obj.get("tools").and_then(Value::as_array).filter(|t| !t.is_empty()) {
        let tools = tools.iter().map(tool).collect::<Result<Vec<_>, _>>()?;
        chat.insert("tools".into(), Value::Array(tools));
    }
    match obj.get("tool_choice") {
        None | Some(Value::Null) => {}
        Some(v @ Value::String(_)) => {
            chat.insert("tool_choice".into(), v.clone());
        }
        Some(v) if v.get("type").and_then(Value::as_str) == Some("function") => {
            chat.insert("tool_choice".into(), json!({"type": "function", "function": {"name": v["name"]}}));
        }
        Some(_) => return Err(native_only("this `tool_choice`")),
    }
    for k in ["parallel_tool_calls", "temperature", "top_p"] {
        if is_set(obj, k) {
            chat.insert(k.into(), obj[k].clone());
        }
    }
    if is_set(obj, "max_output_tokens") {
        chat.insert(token_field.into(), obj["max_output_tokens"].clone());
    }
    if let Some(r) = obj.get("reasoning").and_then(Value::as_object) {
        if r.get("effort").is_some_and(|v| !v.is_null()) {
            chat.insert("reasoning_effort".into(), r["effort"].clone());
        }
        for k in ["summary", "generate_summary"] {
            if r.get(k).is_some_and(|v| !v.is_null()) {
                dropped.push(format!("reasoning.{k}"));
            }
        }
    }
    if let Some(t) = obj.get("text").and_then(Value::as_object) {
        if let Some(f) = t.get("format").filter(|f| !f.is_null()) {
            match f.get("type").and_then(Value::as_str) {
                Some("text") => {}
                Some("json_object") => {
                    chat.insert("response_format".into(), json!({"type": "json_object"}));
                }
                Some("json_schema") => {
                    let mut s = Map::new();
                    for k in ["name", "schema", "strict", "description"] {
                        if let Some(v) = f.get(k).filter(|v| !v.is_null()) {
                            s.insert(k.into(), v.clone());
                        }
                    }
                    chat.insert("response_format".into(), json!({"type": "json_schema", "json_schema": s}));
                }
                _ => return Err("`text.format` must be text, json_object or json_schema".into()),
            }
        }
        if t.get("verbosity").is_some_and(|v| !v.is_null()) {
            dropped.push("text.verbosity".into());
        }
    }
    if obj.get("stream") == Some(&Value::Bool(true)) {
        chat.insert("stream".into(), Value::Bool(true));
    }
    dropped.sort();
    Ok(Translated { chat: Value::Object(chat), dropped, echo })
}

fn text_of(parts: &[Value]) -> String {
    parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n")
}

/// One content part, in chat terms.
fn part(p: &Value) -> Result<Value, String> {
    match p.get("type").and_then(Value::as_str) {
        Some("input_text" | "output_text" | "text") => {
            Ok(json!({"type": "text", "text": p.get("text").and_then(Value::as_str).unwrap_or("")}))
        }
        Some("refusal") => Ok(json!({"type": "text", "text": p.get("refusal").and_then(Value::as_str).unwrap_or("")})),
        Some("input_image") => {
            let Some(url) = p.get("image_url").and_then(Value::as_str) else {
                return Err(native_only("an image by `file_id`"));
            };
            let mut image = json!({"url": url});
            if let Some(d) = p.get("detail").filter(|d| d.is_string()) {
                image["detail"] = d.clone();
            }
            Ok(json!({"type": "image_url", "image_url": image}))
        }
        Some(t) => Err(native_only(&format!("`{t}` content"))),
        None => Err("every content part needs a `type`".into()),
    }
}

/// Content as chat wants it: a string when it is only text (and always for
/// roles whose templates expect one), else a list of parts.
fn content(c: Option<&Value>, as_string: bool) -> Result<Value, String> {
    match c {
        None | Some(Value::Null) => Ok(Value::String(String::new())),
        Some(s @ Value::String(_)) => Ok(s.clone()),
        Some(Value::Array(parts)) => {
            let parts = parts.iter().map(part).collect::<Result<Vec<_>, _>>()?;
            if as_string || parts.iter().all(|p| p["type"] == "text") {
                Ok(Value::String(text_of(&parts)))
            } else {
                Ok(Value::Array(parts))
            }
        }
        Some(_) => Err("message `content` must be a string or a list of parts".into()),
    }
}

fn input_item(item: &Value, messages: &mut Vec<Value>) -> Result<(), String> {
    let kind = item.get("type").and_then(Value::as_str);
    let kind = kind.or_else(|| item.get("role").map(|_| "message")).ok_or("every input item needs a `type`")?;
    match kind {
        "message" => {
            let role = item.get("role").and_then(Value::as_str).unwrap_or("");
            if !matches!(role, "user" | "assistant" | "system" | "developer") {
                return Err(format!("unknown message role `{role}`"));
            }
            let content = content(item.get("content"), role != "user")?;
            messages.push(json!({"role": role, "content": content}));
        }
        "function_call" => {
            let call_id = item.get("call_id").and_then(Value::as_str).ok_or("a `function_call` needs a `call_id`")?;
            let name = item.get("name").and_then(Value::as_str).ok_or("a `function_call` needs a `name`")?;
            let arguments = item.get("arguments").and_then(Value::as_str).unwrap_or("{}");
            let call = json!({"id": call_id, "type": "function", "function": {"name": name, "arguments": arguments}});
            // Calls right after an assistant turn belong to that turn.
            match messages.last_mut().filter(|m| m["role"] == "assistant") {
                Some(last) => {
                    let calls = last.as_object_mut().expect("messages are objects").entry("tool_calls");
                    match calls.or_insert_with(|| Value::Array(vec![])) {
                        Value::Array(a) => a.push(call),
                        other => *other = Value::Array(vec![call]),
                    }
                }
                None => messages.push(json!({"role": "assistant", "content": null, "tool_calls": [call]})),
            }
        }
        "function_call_output" => {
            let call_id =
                item.get("call_id").and_then(Value::as_str).ok_or("a `function_call_output` needs a `call_id`")?;
            let output = content(item.get("output"), true)?;
            messages.push(json!({"role": "tool", "tool_call_id": call_id, "content": output}));
        }
        // Chat has no place for earlier reasoning; the model reasons anew.
        "reasoning" => {}
        "item_reference" => return Err("`item_reference` needs stored responses; send the items themselves".into()),
        other => return Err(native_only(&format!("`{other}` input"))),
    }
    Ok(())
}

fn tool(t: &Value) -> Result<Value, String> {
    match t.get("type").and_then(Value::as_str) {
        Some("function") => {
            let mut f = Map::new();
            for k in ["name", "description", "parameters", "strict"] {
                if let Some(v) = t.get(k).filter(|v| !v.is_null()) {
                    f.insert(k.into(), v.clone());
                }
            }
            Ok(json!({"type": "function", "function": f}))
        }
        Some(other) => Err(native_only(&format!("the `{other}` tool"))),
        None => Err("every tool needs a `type`".into()),
    }
}

// ------------------------------------------------------ answer: from chat

/// What a translated Response object is made of besides the answer.
#[derive(Debug, Clone)]
pub struct Meta {
    /// `resp_` plus random hex, made by the server.
    pub id: String,
    pub created_at: i64,
    /// The id the client asked for (persona or model), never the upstream's.
    pub model: String,
    /// Turn the upstream's reasoning text into `reasoning` items (per model).
    pub reasoning: bool,
    pub echo: Map<String, Value>,
}

impl Meta {
    fn item_id(&self, kind: &str, n: usize) -> String {
        let base = self.id.strip_prefix("resp_").unwrap_or(&self.id);
        format!("{kind}_{base}_{n}")
    }

    fn envelope(&self, status: &str, output: Vec<Value>, usage: Option<Value>, finish: Option<&str>) -> Value {
        let incomplete = match finish {
            Some("length") => json!({"reason": "max_output_tokens"}),
            Some("content_filter") => json!({"reason": "content_filter"}),
            _ => Value::Null,
        };
        let mut r = Map::new();
        r.insert("id".into(), Value::from(self.id.as_str()));
        r.insert("object".into(), Value::from("response"));
        r.insert("created_at".into(), Value::from(self.created_at));
        r.insert("status".into(), Value::from(status));
        r.insert("error".into(), Value::Null);
        r.insert("incomplete_details".into(), incomplete);
        r.insert("model".into(), Value::from(self.model.as_str()));
        r.insert("output".into(), Value::Array(output));
        r.insert("usage".into(), usage.unwrap_or(Value::Null));
        r.insert("store".into(), Value::Bool(false));
        r.insert("previous_response_id".into(), Value::Null);
        for (k, v) in [
            ("instructions", Value::Null),
            ("tools", json!([])),
            ("tool_choice", json!("auto")),
            ("parallel_tool_calls", Value::Bool(true)),
            ("temperature", Value::Null),
            ("top_p", Value::Null),
            ("metadata", json!({})),
        ] {
            r.insert(k.into(), v);
        }
        for (k, v) in &self.echo {
            r.insert(k.clone(), v.clone());
        }
        Value::Object(r)
    }
}

/// `completed`, or `incomplete` when the model was cut off.
fn status_of(finish: Option<&str>) -> &'static str {
    match finish {
        Some("length" | "content_filter") => "incomplete",
        _ => "completed",
    }
}

/// Chat usage in Responses terms.
pub fn usage_out(u: &Value) -> Value {
    let n = |k: &str| u.get(k).and_then(Value::as_i64).unwrap_or(0);
    let deep = |p: &str| u.pointer(p).and_then(Value::as_i64).unwrap_or(0);
    let (input, output) = (n("prompt_tokens"), n("completion_tokens"));
    json!({
        "input_tokens": input,
        "input_tokens_details": {"cached_tokens": deep("/prompt_tokens_details/cached_tokens")},
        "output_tokens": output,
        "output_tokens_details": {"reasoning_tokens": deep("/completion_tokens_details/reasoning_tokens")},
        "total_tokens": u.get("total_tokens").and_then(Value::as_i64).unwrap_or(input + output),
    })
}

/// The upstream's reasoning text: llama.cpp and DeepSeek say
/// `reasoning_content`, OpenRouter `reasoning`.
fn reasoning_of(m: &Value) -> Option<&str> {
    ["reasoning_content", "reasoning"].iter().find_map(|k| m.get(*k).and_then(Value::as_str)).filter(|s| !s.is_empty())
}

fn reasoning_item(id: &str, text: &str) -> Value {
    json!({"type": "reasoning", "id": id, "summary": [], "content": [{"type": "reasoning_text", "text": text}]})
}

fn text_part(text: &str) -> Value {
    json!({"type": "output_text", "text": text, "annotations": []})
}

fn message_item(id: &str, status: &str, content: Vec<Value>) -> Value {
    json!({"type": "message", "id": id, "status": status, "role": "assistant", "content": content})
}

fn call_item(id: &str, call_id: &str, name: &str, arguments: &str, status: &str) -> Value {
    json!({"type": "function_call", "id": id, "call_id": call_id, "name": name, "arguments": arguments, "status": status})
}

/// A chat completion as a Response object.
pub fn from_chat(chat: &Value, meta: &Meta) -> Value {
    let choice = &chat["choices"][0];
    let msg = &choice["message"];
    let mut out = Vec::new();
    if meta.reasoning
        && let Some(r) = reasoning_of(msg)
    {
        out.push(reasoning_item(&meta.item_id("rs", 0), r));
    }
    let mut parts = Vec::new();
    if let Some(t) = msg.get("content").and_then(Value::as_str).filter(|t| !t.is_empty()) {
        parts.push(text_part(t));
    }
    if let Some(r) = msg.get("refusal").and_then(Value::as_str).filter(|t| !t.is_empty()) {
        parts.push(json!({"type": "refusal", "refusal": r}));
    }
    if !parts.is_empty() {
        out.push(message_item(&meta.item_id("msg", 0), "completed", parts));
    }
    for (i, c) in msg.get("tool_calls").and_then(Value::as_array).into_iter().flatten().enumerate() {
        let f = &c["function"];
        out.push(call_item(
            &meta.item_id("fc", i),
            c.get("id").and_then(Value::as_str).unwrap_or(""),
            f.get("name").and_then(Value::as_str).unwrap_or(""),
            f.get("arguments").and_then(Value::as_str).unwrap_or(""),
            "completed",
        ));
    }
    let finish = choice.get("finish_reason").and_then(Value::as_str);
    meta.envelope(status_of(finish), out, chat.get("usage").filter(|u| u.is_object()).map(usage_out), finish)
}

// --------------------------------------------------- answer: the stream

/// The text-like item currently open (at most one at a time).
enum Open {
    None,
    Reasoning(usize),
    Text(usize),
}

/// Turns a chat completions stream into Responses events, chunk by chunk.
/// Every event is emitted as soon as the chunk that causes it arrives.
pub struct Stream {
    meta: Meta,
    seq: u64,
    /// Every output item so far, in output order, kept current.
    items: Vec<Value>,
    open: Open,
    /// The open item's text so far.
    buf: String,
    /// Chat tool call index to its item.
    calls: BTreeMap<u64, usize>,
    usage: Option<Value>,
    finish: Option<String>,
    ended: bool,
}

impl Stream {
    pub fn new(meta: Meta) -> Self {
        Stream {
            meta,
            seq: 0,
            items: vec![],
            open: Open::None,
            buf: String::new(),
            calls: BTreeMap::new(),
            usage: None,
            finish: None,
            ended: false,
        }
    }

    fn event(&mut self, out: &mut Vec<u8>, kind: &str, mut body: Value) {
        body["type"] = Value::from(kind);
        body["sequence_number"] = Value::from(self.seq);
        self.seq += 1;
        out.extend_from_slice(format!("event: {kind}\ndata: {body}\n\n").as_bytes());
    }

    fn response(&self, status: &str) -> Value {
        self.meta.envelope(status, self.items.clone(), self.usage.clone(), self.finish.as_deref())
    }

    /// `response.created` and `response.in_progress`: sent as soon as the
    /// upstream has answered with a stream.
    pub fn start(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        let r = self.response("in_progress");
        self.event(&mut out, "response.created", json!({"response": r}));
        self.event(&mut out, "response.in_progress", json!({"response": r}));
        out
    }

    /// One chat chunk (already parsed). `[DONE]` is not a chunk: call
    /// [`Stream::end`].
    pub fn push(&mut self, chunk: &Value) -> Vec<u8> {
        let mut out = Vec::new();
        if self.ended {
            return out;
        }
        if let Some(err) = chunk.get("error").filter(|e| !e.is_null()) {
            self.close_open(&mut out);
            let message = err.get("message").and_then(Value::as_str).unwrap_or("the upstream reported an error");
            let mut r = self.response("failed");
            r["error"] = json!({"code": "server_error", "message": message});
            self.event(&mut out, "response.failed", json!({"response": r}));
            self.ended = true;
            return out;
        }
        if let Some(u) = chunk.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(usage_out(u));
        }
        let Some(choice) = chunk.get("choices").and_then(|c| c.get(0)) else { return out };
        let delta = &choice["delta"];
        if self.meta.reasoning
            && let Some(r) = reasoning_of(delta)
        {
            let r = r.to_string();
            self.reasoning_delta(&mut out, &r);
        }
        if let Some(t) = delta.get("content").and_then(Value::as_str).filter(|t| !t.is_empty()) {
            let t = t.to_string();
            self.text_delta(&mut out, &t);
        }
        for c in delta.get("tool_calls").and_then(Value::as_array).cloned().into_iter().flatten() {
            self.call_delta(&mut out, &c);
        }
        if let Some(f) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish = Some(f.to_string());
        }
        out
    }

    /// The upstream finished: close what is open and send the final event.
    pub fn end(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.ended {
            return out;
        }
        self.close_open(&mut out);
        for idx in self.calls.values().copied().collect::<Vec<_>>() {
            let (id, args) = (self.items[idx]["id"].clone(), self.items[idx]["arguments"].clone());
            self.items[idx]["status"] = Value::from("completed");
            let name = self.items[idx]["name"].clone();
            self.event(
                &mut out,
                "response.function_call_arguments.done",
                json!({"item_id": id, "output_index": idx, "name": name, "arguments": args}),
            );
            let item = self.items[idx].clone();
            self.event(&mut out, "response.output_item.done", json!({"output_index": idx, "item": item}));
        }
        let status = status_of(self.finish.as_deref());
        let kind = if status == "completed" { "response.completed" } else { "response.incomplete" };
        let r = self.response(status);
        self.event(&mut out, kind, json!({"response": r}));
        self.ended = true;
        out
    }

    fn add_item(&mut self, out: &mut Vec<u8>, item: Value) -> usize {
        let idx = self.items.len();
        self.items.push(item.clone());
        self.event(out, "response.output_item.added", json!({"output_index": idx, "item": item}));
        idx
    }

    fn reasoning_delta(&mut self, out: &mut Vec<u8>, text: &str) {
        let idx = match self.open {
            Open::Reasoning(i) => i,
            _ => {
                self.close_open(out);
                let id = self.meta.item_id("rs", self.items.len());
                let i = self.add_item(out, json!({"type": "reasoning", "id": id, "summary": [], "content": []}));
                self.open = Open::Reasoning(i);
                i
            }
        };
        self.buf.push_str(text);
        let id = self.items[idx]["id"].clone();
        self.event(
            out,
            "response.reasoning_text.delta",
            json!({"item_id": id, "output_index": idx, "content_index": 0, "delta": text}),
        );
    }

    fn text_delta(&mut self, out: &mut Vec<u8>, text: &str) {
        let idx = match self.open {
            Open::Text(i) => i,
            _ => {
                self.close_open(out);
                let id = self.meta.item_id("msg", self.items.len());
                let i = self.add_item(out, message_item(&id, "in_progress", vec![]));
                self.event(
                    out,
                    "response.content_part.added",
                    json!({"item_id": id, "output_index": i, "content_index": 0, "part": text_part("")}),
                );
                self.open = Open::Text(i);
                i
            }
        };
        self.buf.push_str(text);
        let id = self.items[idx]["id"].clone();
        self.event(
            out,
            "response.output_text.delta",
            json!({"item_id": id, "output_index": idx, "content_index": 0, "delta": text, "logprobs": []}),
        );
    }

    fn call_delta(&mut self, out: &mut Vec<u8>, c: &Value) {
        let index = c.get("index").and_then(Value::as_u64).unwrap_or(0);
        let f = &c["function"];
        let idx = match self.calls.get(&index) {
            Some(i) => *i,
            None => {
                self.close_open(out);
                let n = self.items.len();
                let id = self.meta.item_id("fc", n);
                let call_id = c.get("id").and_then(Value::as_str).map(str::to_string);
                let call_id = call_id.unwrap_or_else(|| self.meta.item_id("call", n));
                let name = f.get("name").and_then(Value::as_str).unwrap_or("");
                let i = self.add_item(out, call_item(&id, &call_id, name, "", "in_progress"));
                self.calls.insert(index, i);
                i
            }
        };
        if self.items[idx]["name"] == ""
            && let Some(name) = f.get("name").and_then(Value::as_str)
        {
            self.items[idx]["name"] = Value::from(name);
        }
        if let Some(args) = f.get("arguments").and_then(Value::as_str).filter(|a| !a.is_empty()) {
            let all = format!("{}{args}", self.items[idx]["arguments"].as_str().unwrap_or(""));
            self.items[idx]["arguments"] = Value::from(all);
            let id = self.items[idx]["id"].clone();
            self.event(
                out,
                "response.function_call_arguments.delta",
                json!({"item_id": id, "output_index": idx, "delta": args}),
            );
        }
    }

    fn close_open(&mut self, out: &mut Vec<u8>) {
        let text = std::mem::take(&mut self.buf);
        match std::mem::replace(&mut self.open, Open::None) {
            Open::None => {}
            Open::Reasoning(i) => {
                let id = self.items[i]["id"].clone();
                self.event(
                    out,
                    "response.reasoning_text.done",
                    json!({"item_id": id, "output_index": i, "content_index": 0, "text": text}),
                );
                self.items[i]["content"] = json!([{"type": "reasoning_text", "text": text}]);
                let item = self.items[i].clone();
                self.event(out, "response.output_item.done", json!({"output_index": i, "item": item}));
            }
            Open::Text(i) => {
                let id = self.items[i]["id"].clone();
                self.event(
                    out,
                    "response.output_text.done",
                    json!({"item_id": id, "output_index": i, "content_index": 0, "text": text, "logprobs": []}),
                );
                self.event(
                    out,
                    "response.content_part.done",
                    json!({"item_id": id, "output_index": i, "content_index": 0, "part": text_part(&text)}),
                );
                self.items[i]["content"] = json!([text_part(&text)]);
                self.items[i]["status"] = Value::from("completed");
                let item = self.items[i].clone();
                self.event(out, "response.output_item.done", json!({"output_index": i, "item": item}));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn chat(req: Value) -> Translated {
        to_chat(&req, "max_tokens").unwrap()
    }

    #[test]
    fn a_simple_request_and_the_fields_that_map() {
        let t = to_chat(
            &json!({
                "model": "designer",
                "instructions": "Be brief.",
                "input": "hi",
                "stream": true,
                "max_output_tokens": 500,
                "temperature": 0.3,
                "reasoning": {"effort": "low", "summary": "auto"},
                "text": {"format": {"type": "json_schema", "name": "x", "schema": {"type": "object"}, "strict": true}, "verbosity": "low"},
                "store": false,
                "include": ["reasoning.encrypted_content"],
                "prompt_cache_key": "abc",
                "previous_response_id": null,
                "metadata": {"a": "b"}
            }),
            "max_completion_tokens",
        )
        .unwrap();
        assert_eq!(
            t.chat,
            json!({
                "model": "designer",
                "messages": [{"role": "system", "content": "Be brief."}, {"role": "user", "content": "hi"}],
                "stream": true,
                "max_completion_tokens": 500,
                "temperature": 0.3,
                "reasoning_effort": "low",
                "response_format": {"type": "json_schema", "json_schema": {"name": "x", "schema": {"type": "object"}, "strict": true}}
            })
        );
        assert_eq!(
            t.dropped,
            ["include", "metadata", "prompt_cache_key", "reasoning.summary", "store", "text.verbosity"]
        );
        assert_eq!(t.echo["instructions"], "Be brief.");
        assert_eq!(t.echo["metadata"], json!({"a": "b"}));
        let j = chat(json!({"input": "x", "text": {"format": {"type": "json_object"}}}));
        assert_eq!(j.chat["response_format"], json!({"type": "json_object"}));
    }

    #[test]
    fn input_items_become_chat_messages() {
        let t = chat(json!({"input": [
            {"role": "developer", "content": "Rules."},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Look"}, {"type": "input_image", "image_url": "data:image/png;base64,AA", "detail": "low"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "a"}, {"type": "input_text", "text": "b"}]},
            {"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "zzz"},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Let me check."}]},
            {"type": "function_call", "call_id": "c1", "name": "shell", "arguments": "{\"cmd\":\"ls\"}"},
            {"type": "function_call", "call_id": "c2", "name": "read", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "c1", "output": "a.txt"},
            {"type": "function_call_output", "call_id": "c2", "output": [{"type": "input_text", "text": "x"}]},
            {"type": "function_call", "call_id": "c3", "name": "shell", "arguments": "{}"}
        ]}));
        assert_eq!(
            t.chat["messages"],
            json!([
                {"role": "developer", "content": "Rules."},
                {"role": "user", "content": [{"type": "text", "text": "Look"}, {"type": "image_url", "image_url": {"url": "data:image/png;base64,AA", "detail": "low"}}]},
                {"role": "user", "content": "a\nb"},
                {"role": "assistant", "content": "Let me check.", "tool_calls": [
                    {"id": "c1", "type": "function", "function": {"name": "shell", "arguments": "{\"cmd\":\"ls\"}"}},
                    {"id": "c2", "type": "function", "function": {"name": "read", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "c1", "content": "a.txt"},
                {"role": "tool", "tool_call_id": "c2", "content": "x"},
                {"role": "assistant", "content": null, "tool_calls": [{"id": "c3", "type": "function", "function": {"name": "shell", "arguments": "{}"}}]}
            ])
        );
    }

    #[test]
    fn function_tools_are_reshaped() {
        let t = chat(json!({
            "input": "x",
            "tools": [{"type": "function", "name": "shell", "description": "Run", "parameters": {"type": "object"}, "strict": false}],
            "tool_choice": {"type": "function", "name": "shell"},
            "parallel_tool_calls": false
        }));
        assert_eq!(
            t.chat["tools"],
            json!([{"type": "function", "function": {"name": "shell", "description": "Run", "parameters": {"type": "object"}, "strict": false}}])
        );
        assert_eq!(t.chat["tool_choice"], json!({"type": "function", "function": {"name": "shell"}}));
        assert_eq!(t.chat["parallel_tool_calls"], false);
        assert_eq!(chat(json!({"input": "x", "tool_choice": "required"})).chat["tool_choice"], "required");
    }

    #[test]
    fn what_cannot_be_translated_is_refused_by_name() {
        for (req, says) in [
            (json!({"input": "x", "previous_response_id": "resp_1"}), "previous_response_id"),
            (json!({"input": "x", "conversation": "conv_1"}), "conversation"),
            (json!({"input": "x", "background": true}), "background"),
            (json!({"input": "x", "prompt": {"id": "p"}}), "prompt"),
            (json!({"input": [{"type": "item_reference", "id": "msg_1"}]}), "item_reference"),
            (json!({"input": "x", "tools": [{"type": "web_search"}]}), "`web_search` tool"),
            (json!({"input": "x", "tools": [{"type": "custom", "name": "apply_patch"}]}), "`custom` tool"),
            (json!({"input": [{"role": "user", "content": [{"type": "input_image", "file_id": "f"}]}]}), "file_id"),
            (json!({"input": [{"role": "user", "content": [{"type": "input_file", "file_id": "f"}]}]}), "`input_file`"),
            (json!({"input": [{"type": "local_shell_call", "call_id": "c"}]}), "`local_shell_call`"),
            (json!({"input": "x", "tool_choice": {"type": "allowed_tools"}}), "tool_choice"),
            (json!({"instructions": "only this"}), "`input` is required"),
            (json!({"input": 3}), "`input` must be"),
        ] {
            let e = to_chat(&req, "max_tokens").unwrap_err();
            assert!(e.contains(says), "{req}: {e}");
        }
        assert!(to_chat(&json!({"input": "x", "background": false}), "max_tokens").is_ok());
    }

    fn meta(reasoning: bool) -> Meta {
        let echo = json!({"tools": [{"type": "function", "name": "shell"}]}).as_object().unwrap().clone();
        Meta { id: "resp_ab12".into(), created_at: 1700000000, model: "designer".into(), reasoning, echo }
    }

    #[test]
    fn a_chat_answer_becomes_a_response() {
        let answer = json!({
            "model": "qwen3-30b",
            "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
                "role": "assistant",
                "content": "Checking.",
                "reasoning_content": "They want ls.",
                "tool_calls": [{"id": "call_9", "type": "function", "function": {"name": "shell", "arguments": "{\"cmd\":\"ls\"}"}}]
            }}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15, "prompt_tokens_details": {"cached_tokens": 4}, "completion_tokens_details": {"reasoning_tokens": 3}}
        });
        let r = from_chat(&answer, &meta(true));
        assert_eq!(r["id"], "resp_ab12");
        assert_eq!(r["object"], "response");
        assert_eq!(r["model"], "designer", "what the client asked for");
        assert_eq!(r["status"], "completed");
        assert_eq!(r["tools"], json!([{"type": "function", "name": "shell"}]), "echoed");
        assert_eq!(
            r["output"],
            json!([
                {"type": "reasoning", "id": "rs_ab12_0", "summary": [], "content": [{"type": "reasoning_text", "text": "They want ls."}]},
                {"type": "message", "id": "msg_ab12_0", "status": "completed", "role": "assistant", "content": [{"type": "output_text", "text": "Checking.", "annotations": []}]},
                {"type": "function_call", "id": "fc_ab12_0", "call_id": "call_9", "name": "shell", "arguments": "{\"cmd\":\"ls\"}", "status": "completed"}
            ])
        );
        assert_eq!(
            r["usage"],
            json!({"input_tokens": 10, "input_tokens_details": {"cached_tokens": 4}, "output_tokens": 5, "output_tokens_details": {"reasoning_tokens": 3}, "total_tokens": 15})
        );
        let plain = from_chat(&answer, &meta(false));
        assert_eq!(plain["output"].as_array().unwrap().len(), 2, "reasoning off for this model");
    }

    #[test]
    fn cut_off_and_refused_answers() {
        let cut = json!({"choices": [{"finish_reason": "length", "message": {"content": "Once upon"}}]});
        let r = from_chat(&cut, &meta(true));
        assert_eq!(r["status"], "incomplete");
        assert_eq!(r["incomplete_details"], json!({"reason": "max_output_tokens"}));
        assert_eq!(r["usage"], Value::Null);
        let no = json!({"choices": [{"finish_reason": "stop", "message": {"content": null, "refusal": "No."}}]});
        assert_eq!(from_chat(&no, &meta(true))["output"][0]["content"], json!([{"type": "refusal", "refusal": "No."}]));
    }

    /// The emitted events as (event line, data), checking they agree and
    /// that sequence numbers count up from `from`.
    fn events(bytes: &[u8], from: &mut u64) -> Vec<(String, Value)> {
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        text.split_terminator("\n\n")
            .map(|e| {
                let (ev, data) = e.split_once('\n').unwrap();
                let kind = ev.strip_prefix("event: ").unwrap().to_string();
                let v: Value = serde_json::from_str(data.strip_prefix("data: ").unwrap()).unwrap();
                assert_eq!(v["type"], kind.as_str());
                assert_eq!(v["sequence_number"], *from, "{kind}");
                *from += 1;
                (kind, v)
            })
            .collect()
    }

    fn run(chunks: &[Value], reasoning: bool) -> Vec<(String, Value)> {
        let mut s = Stream::new(meta(reasoning));
        let mut seq = 0;
        let mut all = events(&s.start(), &mut seq);
        for c in chunks {
            all.extend(events(&s.push(c), &mut seq));
        }
        all.extend(events(&s.end(), &mut seq));
        assert!(s.end().is_empty() && s.push(&chunks[0]).is_empty(), "nothing after the end");
        all
    }

    fn kinds(e: &[(String, Value)]) -> Vec<&str> {
        e.iter().map(|(k, _)| k.trim_start_matches("response.")).collect()
    }

    fn delta(v: Value) -> Value {
        json!({"choices": [{"index": 0, "delta": v}]})
    }

    #[test]
    fn a_text_stream() {
        let e = run(
            &[
                delta(json!({"role": "assistant", "content": ""})),
                delta(json!({"content": "Hel"})),
                delta(json!({"content": "lo"})),
                json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
                json!({"choices": [], "usage": {"prompt_tokens": 5, "completion_tokens": 2}}),
            ],
            true,
        );
        assert_eq!(
            kinds(&e),
            [
                "created",
                "in_progress",
                "output_item.added",
                "content_part.added",
                "output_text.delta",
                "output_text.delta",
                "output_text.done",
                "content_part.done",
                "output_item.done",
                "completed"
            ]
        );
        assert_eq!(e[0].1["response"]["status"], "in_progress");
        assert_eq!(e[4].1["delta"], "Hel");
        assert_eq!(e[4].1["item_id"], "msg_ab12_0");
        assert_eq!(e[6].1["text"], "Hello");
        let done = &e[9].1["response"];
        assert_eq!(done["status"], "completed");
        assert_eq!(done["output"][0]["content"][0]["text"], "Hello");
        assert_eq!(done["output"][0]["status"], "completed");
        assert_eq!(done["usage"]["input_tokens"], 5);
        assert_eq!(done["usage"]["total_tokens"], 7);
    }

    #[test]
    fn reasoning_then_text_and_reasoning_switched_off() {
        let chunks = [
            delta(json!({"reasoning_content": "Hmm"})),
            delta(json!({"reasoning_content": "."})),
            delta(json!({"content": "Yes"})),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "length"}]}),
        ];
        let e = run(&chunks, true);
        assert_eq!(
            kinds(&e),
            [
                "created",
                "in_progress",
                "output_item.added",
                "reasoning_text.delta",
                "reasoning_text.delta",
                "reasoning_text.done",
                "output_item.done",
                "output_item.added",
                "content_part.added",
                "output_text.delta",
                "output_text.done",
                "content_part.done",
                "output_item.done",
                "incomplete"
            ]
        );
        assert_eq!(e[5].1["text"], "Hmm.");
        assert_eq!(e[7].1["output_index"], 1);
        let last = &e[13].1["response"];
        assert_eq!(last["incomplete_details"]["reason"], "max_output_tokens");
        assert_eq!(last["output"][0]["content"], json!([{"type": "reasoning_text", "text": "Hmm."}]));
        let off = run(&chunks, false);
        assert!(!kinds(&off).iter().any(|k| k.starts_with("reasoning")));
        assert_eq!(off.last().unwrap().1["response"]["output"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn two_tool_calls_with_split_arguments() {
        let call = |i: u64, id: Option<&str>, name: Option<&str>, args: &str| {
            let mut f = json!({"arguments": args});
            if let Some(n) = name {
                f["name"] = json!(n);
            }
            let mut c = json!({"index": i, "function": f});
            if let Some(id) = id {
                c["id"] = json!(id);
                c["type"] = json!("function");
            }
            delta(json!({"tool_calls": [c]}))
        };
        let e = run(
            &[
                delta(json!({"content": "On it."})),
                call(0, Some("call_a"), Some("shell"), ""),
                call(0, None, None, "{\"cmd\":"),
                call(0, None, None, "\"ls\"}"),
                call(1, Some("call_b"), Some("read"), "{}"),
                json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
            ],
            true,
        );
        assert_eq!(
            kinds(&e),
            [
                "created",
                "in_progress",
                "output_item.added",
                "content_part.added",
                "output_text.delta",
                "output_text.done",
                "content_part.done",
                "output_item.done",
                "output_item.added",
                "function_call_arguments.delta",
                "function_call_arguments.delta",
                "output_item.added",
                "function_call_arguments.delta",
                "function_call_arguments.done",
                "output_item.done",
                "function_call_arguments.done",
                "output_item.done",
                "completed"
            ]
        );
        assert_eq!(e[8].1["item"]["call_id"], "call_a");
        assert_eq!(e[13].1["arguments"], "{\"cmd\":\"ls\"}");
        assert_eq!(e[13].1["output_index"], 1);
        let out = &e[17].1["response"]["output"];
        assert_eq!(
            out[1],
            json!({"type": "function_call", "id": "fc_ab12_1", "call_id": "call_a", "name": "shell", "arguments": "{\"cmd\":\"ls\"}", "status": "completed"})
        );
        assert_eq!(out[2]["call_id"], "call_b");
        assert_eq!(out[2]["arguments"], "{}");
    }

    #[test]
    fn an_error_chunk_fails_the_response() {
        let e = run(
            &[delta(json!({"content": "Par"})), json!({"error": {"message": "provider overloaded", "code": 502}})],
            true,
        );
        let k = kinds(&e);
        assert_eq!(k.last(), Some(&"failed"));
        assert!(k.contains(&"output_item.done"), "open items are closed first");
        let r = &e.last().unwrap().1["response"];
        assert_eq!(r["status"], "failed");
        assert_eq!(r["error"]["message"], "provider overloaded");
    }

    #[test]
    fn native_prompt_goes_into_instructions() {
        let mut b = json!({"instructions": "Client rules.", "input": [{"role": "system", "content": "s"}, {"role": "user", "content": "hi"}]})
            .as_object()
            .unwrap()
            .clone();
        let mut p = b.clone();
        apply_native_prompt(&mut p, "Persona.", PromptMode::Prepend);
        assert_eq!(p["instructions"], "Persona.\n\nClient rules.");
        assert_eq!(p["input"].as_array().unwrap().len(), 2);
        apply_native_prompt(&mut b, "Persona.", PromptMode::Replace);
        assert_eq!(b["instructions"], "Persona.");
        assert_eq!(b["input"], json!([{"role": "user", "content": "hi"}]));
        let mut empty = Map::new();
        apply_native_prompt(&mut empty, "Persona.", PromptMode::Prepend);
        assert_eq!(empty["instructions"], "Persona.");
    }

    #[test]
    fn native_parameters_use_responses_names() {
        let persona = Params::from_json(&json!({
            "max_tokens": {"mode": "lock", "value": 100},
            "reasoning_effort": {"mode": "default", "value": "high"},
            "seed": {"mode": "lock", "value": 1},
            "stop": {"mode": "lock", "value": "x"}
        }));
        let model = Params::from_json(&json!({"temperature": {"mode": "default", "value": 0.5}}));
        let mut b = json!({"max_output_tokens": 9000, "reasoning": {"summary": "auto"}}).as_object().unwrap().clone();
        fill_native(&mut b, Some(&persona), &model);
        assert_eq!(
            Value::Object(b),
            json!({"max_output_tokens": 100, "reasoning": {"summary": "auto", "effort": "high"}, "temperature": 0.5})
        );
        let mut sent = json!({"reasoning": {"effort": "low"}}).as_object().unwrap().clone();
        fill_native(&mut sent, Some(&persona), &model);
        assert_eq!(sent["reasoning"]["effort"], "low", "a default leaves what the client sent");
    }
}
