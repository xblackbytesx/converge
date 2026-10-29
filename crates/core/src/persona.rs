//! Personas on the wire: the system prompt and the parameter rules, applied
//! to a chat completions body. Pure, so every rule has a golden test
//! (AGENTS.md, M3 plan).

use std::collections::BTreeMap;

use serde_json::{Map, Value};

/// The parameters a persona or model can set. A fixed list: each one means
/// the same on every OpenAI-compatible upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Param {
    Temperature,
    TopP,
    MaxTokens,
    PresencePenalty,
    FrequencyPenalty,
    Seed,
    Stop,
    ReasoningEffort,
}

/// `reasoning_effort` values the form offers and the store accepts.
pub const EFFORTS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh"];

/// The two names clients use for the output token limit. Newer OpenAI
/// models refuse the first; llama.cpp and many others only know it.
pub const TOKEN_FIELDS: [&str; 2] = ["max_tokens", "max_completion_tokens"];

impl Param {
    pub const ALL: [Param; 8] = [
        Param::Temperature,
        Param::TopP,
        Param::MaxTokens,
        Param::PresencePenalty,
        Param::FrequencyPenalty,
        Param::Seed,
        Param::Stop,
        Param::ReasoningEffort,
    ];

    /// The request field, and the key in stored params.
    pub fn key(self) -> &'static str {
        match self {
            Param::Temperature => "temperature",
            Param::TopP => "top_p",
            Param::MaxTokens => "max_tokens",
            Param::PresencePenalty => "presence_penalty",
            Param::FrequencyPenalty => "frequency_penalty",
            Param::Seed => "seed",
            Param::Stop => "stop",
            Param::ReasoningEffort => "reasoning_effort",
        }
    }

    pub fn from_key(key: &str) -> Option<Param> {
        Param::ALL.into_iter().find(|p| p.key() == key)
    }

    /// What the form says next to the field.
    pub fn hint(self) -> &'static str {
        match self {
            Param::Temperature => "0 to 2",
            Param::TopP => "0 to 1",
            Param::MaxTokens => "output tokens, also sent as max_completion_tokens",
            Param::PresencePenalty | Param::FrequencyPenalty => "-2 to 2",
            Param::Seed => "a whole number",
            Param::Stop => "one stop text, or a JSON list of up to 4",
            Param::ReasoningEffort => "none, minimal, low, medium, high or xhigh",
        }
    }

    /// Reads what an admin typed into the form.
    pub fn parse(self, text: &str) -> Result<Value, String> {
        let t = text.trim();
        let label = self.key();
        let number = |lo: f64, hi: f64| -> Result<Value, String> {
            let n: f64 = t.parse().map_err(|_| format!("{label} must be a number"))?;
            if !(lo..=hi).contains(&n) {
                return Err(format!("{label} must be between {lo} and {hi}"));
            }
            serde_json::Number::from_f64(n).map(Value::Number).ok_or_else(|| format!("{label} must be a number"))
        };
        match self {
            Param::Temperature => number(0.0, 2.0),
            Param::TopP => number(0.0, 1.0),
            Param::PresencePenalty | Param::FrequencyPenalty => number(-2.0, 2.0),
            Param::MaxTokens => match t.parse::<i64>() {
                Ok(n) if (1..=10_000_000).contains(&n) => Ok(Value::from(n)),
                _ => Err(format!("{label} must be a whole number from 1")),
            },
            Param::Seed => t.parse::<i64>().map(Value::from).map_err(|_| format!("{label} must be a whole number")),
            Param::Stop => {
                let v = match serde_json::from_str::<Value>(t) {
                    Ok(v @ (Value::Array(_) | Value::String(_))) => v,
                    _ => Value::String(text.to_string()),
                };
                if self.fits(&v) { Ok(v) } else { Err(format!("{label} takes 1 to 4 non-empty texts")) }
            }
            Param::ReasoningEffort => {
                let w = t.to_lowercase();
                if EFFORTS.contains(&w.as_str()) {
                    Ok(Value::String(w))
                } else {
                    Err(format!("{label} must be one of {}", EFFORTS.join(", ")))
                }
            }
        }
    }

    /// Whether a stored value is still acceptable (rows are checked again on
    /// load, so a hand-edited row cannot put garbage on the wire).
    pub fn fits(self, v: &Value) -> bool {
        let num = |lo: f64, hi: f64| v.as_f64().is_some_and(|n| (lo..=hi).contains(&n));
        match self {
            Param::Temperature => num(0.0, 2.0),
            Param::TopP => num(0.0, 1.0),
            Param::PresencePenalty | Param::FrequencyPenalty => num(-2.0, 2.0),
            Param::MaxTokens => v.as_i64().is_some_and(|n| n >= 1),
            Param::Seed => v.as_i64().is_some(),
            Param::Stop => match v {
                Value::String(s) => !s.is_empty(),
                Value::Array(a) => {
                    (1..=4).contains(&a.len()) && a.iter().all(|s| s.as_str().is_some_and(|s| !s.is_empty()))
                }
                _ => false,
            },
            Param::ReasoningEffort => v.as_str().is_some_and(|s| EFFORTS.contains(&s)),
        }
    }

    /// A stored value as the form shows it again.
    pub fn show(v: &Value) -> String {
        match v {
            Value::String(s) => s.clone(),
            _ => v.to_string(),
        }
    }
}

/// How a setting applies: fill in what the client left out, or always win.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Default,
    Lock,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Default => "default",
            Mode::Lock => "lock",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Setting {
    pub mode: Mode,
    pub value: Value,
}

/// A persona's (or a model's) settings. Parameters not in the map are off.
/// Stored as `{"temperature": {"mode": "lock", "value": 0.2}, ...}`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Params(pub BTreeMap<Param, Setting>);

impl Params {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get(&self, p: Param) -> Option<&Setting> {
        self.0.get(&p)
    }

    pub fn to_json(&self) -> Value {
        let m: Map<String, Value> = self
            .0
            .iter()
            .map(|(p, s)| {
                let mut o = Map::new();
                o.insert("mode".into(), Value::from(s.mode.as_str()));
                o.insert("value".into(), s.value.clone());
                (p.key().to_string(), Value::Object(o))
            })
            .collect();
        Value::Object(m)
    }

    /// Reads stored params. Unknown names, unknown modes and values that no
    /// longer fit are left out rather than failing the whole load.
    pub fn from_json(v: &Value) -> Params {
        let mut out = BTreeMap::new();
        for (k, s) in v.as_object().into_iter().flatten() {
            let Some(p) = Param::from_key(k) else { continue };
            let mode = match s.get("mode").and_then(Value::as_str) {
                Some("default") => Mode::Default,
                Some("lock") => Mode::Lock,
                _ => continue,
            };
            let Some(value) = s.get("value").filter(|v| p.fits(v)) else { continue };
            out.insert(p, Setting { mode, value: value.clone() });
        }
        Params(out)
    }

    /// Reads the parameter table of a form: `{key}_mode` (`off`, `default`,
    /// `lock`) and `{key}_value` per parameter. Without `locks` (a model's
    /// defaults) there is no mode field: a value means default, blank means
    /// off.
    pub fn from_form<'a>(field: impl Fn(&str) -> Option<&'a str>, locks: bool) -> Result<Params, String> {
        let mut out = BTreeMap::new();
        for p in Param::ALL {
            let value = field(&format!("{}_value", p.key())).unwrap_or("");
            let mode = if locks {
                match field(&format!("{}_mode", p.key())).unwrap_or("off") {
                    "default" => Mode::Default,
                    "lock" => Mode::Lock,
                    _ => continue,
                }
            } else if value.trim().is_empty() {
                continue;
            } else {
                Mode::Default
            };
            if value.trim().is_empty() {
                return Err(format!("give {} a value, or set it to off", p.key()));
            }
            out.insert(p, Setting { mode, value: p.parse(value)? });
        }
        Ok(Params(out))
    }

    /// A short summary for lists and history: `temperature 0.2 (lock)`.
    pub fn summary(&self) -> String {
        if self.0.is_empty() {
            return "none".into();
        }
        self.0
            .iter()
            .map(|(p, s)| {
                let lock = if s.mode == Mode::Lock { " (lock)" } else { "" };
                format!("{} {}{lock}", p.key(), Param::show(&s.value))
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Where the persona prompt goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMode {
    /// Into the first message, ahead of the client's own system message.
    Prepend,
    /// The client's system and developer messages are dropped.
    Replace,
}

impl PromptMode {
    pub fn parse(s: &str) -> PromptMode {
        if s == "replace" { PromptMode::Replace } else { PromptMode::Prepend }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            PromptMode::Prepend => "prepend",
            PromptMode::Replace => "replace",
        }
    }
}

fn is_system(m: &Value) -> bool {
    matches!(m.get("role").and_then(Value::as_str), Some("system" | "developer"))
}

fn system_message(prompt: &str) -> Value {
    let mut m = Map::new();
    m.insert("role".into(), Value::from("system"));
    m.insert("content".into(), Value::from(prompt));
    Value::Object(m)
}

/// Puts the persona prompt into a chat completions body.
///
/// Prepend merges into the client's leading `system` or `developer` message
/// (persona text, a blank line, the client's) instead of adding a second
/// one: several llama.cpp chat templates refuse a second system message, or
/// one that is not first. A body without `messages` gets one; a `messages`
/// that is not a list is left for the upstream to refuse.
pub fn apply_prompt(body: &mut Map<String, Value>, prompt: &str, mode: PromptMode) {
    let messages = body.entry("messages").or_insert_with(|| Value::Array(vec![]));
    let Some(messages) = messages.as_array_mut() else { return };
    let has_prompt = !prompt.trim().is_empty();
    match mode {
        PromptMode::Replace => {
            messages.retain(|m| !is_system(m));
            if has_prompt {
                messages.insert(0, system_message(prompt));
            }
        }
        PromptMode::Prepend if !has_prompt => {}
        PromptMode::Prepend => {
            let Some(first) = messages.first_mut().filter(|m| is_system(m)) else {
                messages.insert(0, system_message(prompt));
                return;
            };
            let content = first.as_object_mut().expect("a message with a role is an object").entry("content");
            let content = content.or_insert(Value::Null);
            match content {
                Value::String(s) if s.trim().is_empty() => *s = prompt.to_string(),
                Value::String(s) => *s = format!("{prompt}\n\n{s}"),
                Value::Array(parts) => {
                    let mut part = Map::new();
                    part.insert("type".into(), Value::from("text"));
                    part.insert("text".into(), Value::from(format!("{prompt}\n\n")));
                    parts.insert(0, Value::Object(part));
                }
                other => *other = Value::from(prompt),
            }
        }
    }
}

/// The value one parameter gets, per the rule: a persona lock wins; else
/// what the client sent (`sent`, None here); else the persona default; else
/// the model default. Shared by chat and native Responses, which name the
/// fields differently.
pub fn pick<'a>(p: Param, sent: bool, persona: Option<&'a Params>, model: &'a Params) -> Option<&'a Value> {
    match persona.and_then(|x| x.get(p)) {
        Some(s) if s.mode == Mode::Lock => Some(&s.value),
        _ if sent => None,
        Some(s) => Some(&s.value),
        None => model.get(p).map(|s| &s.value),
    }
}

/// Fills in parameters on a chat body, per [`pick`].
///
/// `max_tokens` and `max_completion_tokens` count as one. When the client
/// sent either, defaults leave it alone and a lock overwrites whichever it
/// sent; when it sent neither, the value goes out as `token_field`, which
/// the connection declares.
pub fn fill(body: &mut Map<String, Value>, persona: Option<&Params>, model: &Params, token_field: &str) {
    for p in Param::ALL {
        let names: &[&str] = if p == Param::MaxTokens { &TOKEN_FIELDS } else { &[p.key()] };
        let sent: Vec<&str> = names.iter().copied().filter(|k| body.get(*k).is_some_and(|v| !v.is_null())).collect();
        let Some(value) = pick(p, !sent.is_empty(), persona, model).cloned() else { continue };
        if sent.is_empty() {
            let field = if p == Param::MaxTokens { token_field } else { p.key() };
            body.insert(field.to_string(), value);
        } else {
            for k in sent {
                body.insert(k.to_string(), value.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn prompted(body: Value, mode: PromptMode) -> Value {
        let mut b = obj(body);
        apply_prompt(&mut b, "Be brief.", mode);
        Value::Object(b)
    }

    #[test]
    fn prepend_adds_a_system_message_when_the_client_has_none() {
        let b = prompted(json!({"messages": [{"role": "user", "content": "hi"}]}), PromptMode::Prepend);
        assert_eq!(
            b["messages"],
            json!([{"role": "system", "content": "Be brief."}, {"role": "user", "content": "hi"}])
        );
    }

    #[test]
    fn prepend_merges_into_the_leading_system_or_developer_message() {
        for role in ["system", "developer"] {
            let b = prompted(
                json!({"messages": [{"role": role, "content": "You are in Zed."}, {"role": "user", "content": "hi"}]}),
                PromptMode::Prepend,
            );
            assert_eq!(
                b["messages"],
                json!([{"role": role, "content": "Be brief.\n\nYou are in Zed."}, {"role": "user", "content": "hi"}]),
                "{role}"
            );
        }
    }

    #[test]
    fn prepend_into_array_content_and_odd_content() {
        let b = prompted(
            json!({"messages": [{"role": "system", "content": [{"type": "text", "text": "Client."}]}]}),
            PromptMode::Prepend,
        );
        assert_eq!(
            b["messages"][0]["content"],
            json!([{"type": "text", "text": "Be brief.\n\n"}, {"type": "text", "text": "Client."}])
        );
        let b = prompted(json!({"messages": [{"role": "system", "content": "  "}]}), PromptMode::Prepend);
        assert_eq!(b["messages"][0]["content"], "Be brief.");
        let b = prompted(json!({"messages": [{"role": "system"}]}), PromptMode::Prepend);
        assert_eq!(b["messages"][0]["content"], "Be brief.");
    }

    #[test]
    fn prepend_leaves_a_later_system_message_alone() {
        let b = prompted(
            json!({"messages": [{"role": "user", "content": "hi"}, {"role": "system", "content": "late"}]}),
            PromptMode::Prepend,
        );
        assert_eq!(b["messages"].as_array().unwrap().len(), 3);
        assert_eq!(b["messages"][0], json!({"role": "system", "content": "Be brief."}));
        assert_eq!(b["messages"][2]["content"], "late");
    }

    #[test]
    fn replace_drops_every_client_system_and_developer_message() {
        let b = prompted(
            json!({"messages": [
                {"role": "system", "content": "a"},
                {"role": "user", "content": "hi"},
                {"role": "developer", "content": "b"},
                {"role": "assistant", "content": "yo"}
            ]}),
            PromptMode::Replace,
        );
        assert_eq!(
            b["messages"],
            json!([
                {"role": "system", "content": "Be brief."},
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "yo"}
            ])
        );
    }

    #[test]
    fn no_messages_and_empty_prompts() {
        assert_eq!(
            prompted(json!({}), PromptMode::Prepend)["messages"],
            json!([{"role": "system", "content": "Be brief."}])
        );
        assert_eq!(prompted(json!({"messages": "x"}), PromptMode::Replace)["messages"], "x", "not ours to fix");
        let mut b = obj(json!({"messages": [{"role": "system", "content": "a"}, {"role": "user", "content": "hi"}]}));
        apply_prompt(&mut b, " ", PromptMode::Prepend);
        assert_eq!(b["messages"].as_array().unwrap().len(), 2, "an empty prompt adds nothing");
        apply_prompt(&mut b, "", PromptMode::Replace);
        assert_eq!(b["messages"], json!([{"role": "user", "content": "hi"}]), "replace still drops theirs");
    }

    fn params(v: Value) -> Params {
        let p = Params::from_json(&v);
        assert_eq!(p.0.len(), v.as_object().unwrap().len(), "every setting loads: {v}");
        p
    }

    fn filled(client: Value, persona: Option<Value>, model: Value, field: &str) -> Value {
        let mut b = obj(client);
        let persona = persona.map(params);
        fill(&mut b, persona.as_ref(), &params(model), field);
        Value::Object(b)
    }

    #[test]
    fn lock_beats_client_beats_persona_default_beats_model_default() {
        let persona = json!({
            "temperature": {"mode": "lock", "value": 0.2},
            "top_p": {"mode": "default", "value": 0.9},
            "seed": {"mode": "default", "value": 7}
        });
        let model = json!({
            "top_p": {"mode": "default", "value": 0.5},
            "presence_penalty": {"mode": "default", "value": 1.0},
            "seed": {"mode": "default", "value": 1}
        });
        let b = filled(json!({"temperature": 1.5, "seed": 42}), Some(persona.clone()), model.clone(), "max_tokens");
        assert_eq!(b, json!({"temperature": 0.2, "seed": 42, "top_p": 0.9, "presence_penalty": 1.0}));
        let b = filled(json!({"top_p": null}), Some(persona), model, "max_tokens");
        assert_eq!(b["top_p"], 0.9, "null counts as not sent");
    }

    #[test]
    fn model_defaults_apply_without_a_persona() {
        let model = json!({"temperature": {"mode": "default", "value": 0.7}});
        assert_eq!(filled(json!({}), None, model.clone(), "max_tokens"), json!({"temperature": 0.7}));
        assert_eq!(filled(json!({"temperature": 1}), None, model, "max_tokens"), json!({"temperature": 1}));
    }

    #[test]
    fn the_two_token_limits_count_as_one() {
        let def = json!({"max_tokens": {"mode": "default", "value": 500}});
        let lock = json!({"max_tokens": {"mode": "lock", "value": 100}});
        let m = json!({});
        assert_eq!(filled(json!({}), Some(def.clone()), m.clone(), "max_tokens"), json!({"max_tokens": 500}));
        assert_eq!(
            filled(json!({}), Some(def.clone()), m.clone(), "max_completion_tokens"),
            json!({"max_completion_tokens": 500}),
            "the connection's field when the client sent neither"
        );
        assert_eq!(
            filled(json!({"max_completion_tokens": 9}), Some(def), m.clone(), "max_tokens"),
            json!({"max_completion_tokens": 9}),
            "either one counts as sent"
        );
        assert_eq!(
            filled(json!({"max_completion_tokens": 9000}), Some(lock.clone()), m.clone(), "max_tokens"),
            json!({"max_completion_tokens": 100}),
            "a lock writes the one the client used"
        );
        assert_eq!(
            filled(json!({"max_tokens": 1, "max_completion_tokens": 2}), Some(lock), m, "max_tokens"),
            json!({"max_tokens": 100, "max_completion_tokens": 100})
        );
    }

    #[test]
    fn stored_params_round_trip_and_bad_rows_are_skipped() {
        let p = params(json!({
            "temperature": {"mode": "lock", "value": 0.2},
            "stop": {"mode": "default", "value": ["\n\n", "END"]},
            "reasoning_effort": {"mode": "default", "value": "low"}
        }));
        assert_eq!(Params::from_json(&p.to_json()), p);
        let bad = Params::from_json(&json!({
            "temperature": {"mode": "lock", "value": 9},
            "top_p": {"mode": "maybe", "value": 0.5},
            "logit_bias": {"mode": "lock", "value": {}},
            "seed": {"mode": "default", "value": 1}
        }));
        assert_eq!(bad.0.keys().copied().collect::<Vec<_>>(), [Param::Seed]);
        assert_eq!(Params::from_json(&json!(null)), Params::default());
    }

    #[test]
    fn form_values_are_parsed_and_checked() {
        let form = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| *v)
        };
        let p = Params::from_form(
            form(&[
                ("temperature_mode", "lock"),
                ("temperature_value", " 0.3 "),
                ("top_p_mode", "off"),
                ("top_p_value", "nonsense is ignored when off"),
                ("stop_mode", "default"),
                ("stop_value", "###"),
                ("max_tokens_mode", "default"),
                ("max_tokens_value", "2048"),
            ]),
            true,
        )
        .unwrap();
        assert_eq!(p.summary(), "temperature 0.3 (lock), max_tokens 2048, stop ###");
        assert_eq!(p.get(Param::Stop).unwrap().value, json!("###"));

        let models = Params::from_form(form(&[("seed_value", "5"), ("temperature_value", "")]), false).unwrap();
        assert_eq!(models.summary(), "seed 5");

        for bad in [
            &[("temperature_mode", "default"), ("temperature_value", "")][..],
            &[("temperature_mode", "lock"), ("temperature_value", "3")],
            &[("max_tokens_mode", "lock"), ("max_tokens_value", "0")],
            &[("seed_mode", "lock"), ("seed_value", "1.5")],
            &[("stop_mode", "lock"), ("stop_value", "[\"a\",\"b\",\"c\",\"d\",\"e\"]")],
            &[("reasoning_effort_mode", "lock"), ("reasoning_effort_value", "max")],
        ] {
            let f = move |k: &str| bad.iter().find(|(n, _)| *n == k).map(|(_, v)| *v);
            assert!(Params::from_form(f, true).is_err(), "{bad:?}");
        }
        assert_eq!(Param::Stop.parse("[\"a\", \"b\"]").unwrap(), json!(["a", "b"]));
        assert_eq!(Param::ReasoningEffort.parse(" High ").unwrap(), json!("high"));
    }
}
