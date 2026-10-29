//! Personas (layer 2): their own section, never mixed into Connections
//! (decision 9). Admins create and edit them; users see the ones their
//! grants allow. Every write reloads the snapshot before answering.

use std::collections::BTreeMap;

use askama::Template;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use converge_core::persona::{Param, Params, PromptMode};
use serde_json::Value;
use uuid::Uuid;

use super::session::{AdminUser, WebUser};
use super::{Nav, html};
use crate::AppState;
use crate::error::{ApiError, WebError};
use crate::personas::{self as store, By, PersonaInput};

type Page = Result<Response, WebError>;

fn bad(e: anyhow::Error) -> WebError {
    ApiError::bad(e.to_string()).into()
}

/// One row of a parameter table, for the persona form and model defaults.
pub struct ParamRow {
    pub key: &'static str,
    pub hint: &'static str,
    /// `off`, `default` or `lock`.
    pub mode: &'static str,
    pub value: String,
}

pub fn param_rows(p: &Params) -> Vec<ParamRow> {
    Param::ALL
        .into_iter()
        .map(|k| {
            let s = p.get(k);
            ParamRow {
                key: k.key(),
                hint: k.hint(),
                mode: s.map_or("off", |s| s.mode.as_str()),
                value: s.map(|s| Param::show(&s.value)).unwrap_or_default(),
            }
        })
        .collect()
}

struct Card {
    id: Uuid,
    public_id: String,
    title: String,
    description: String,
    /// Admins only: the model it runs on.
    model: String,
    paused: bool,
    enabled: bool,
}

#[derive(Template)]
#[template(path = "personas.html")]
struct ListPage {
    nav: Nav,
    admin: bool,
    cards: Vec<Card>,
}

pub async fn page(State(st): State<AppState>, user: WebUser) -> Page {
    let cards = if user.is_admin() {
        store::list(&st.pool)
            .await?
            .into_iter()
            .map(|p| Card {
                paused: !p.model_live,
                enabled: p.enabled,
                model: p.model_public_id,
                id: p.id,
                public_id: p.public_id,
                title: p.title,
                description: p.description,
            })
            .collect()
    } else {
        let snap = st.gateway.load();
        let mut v: Vec<Card> = snap
            .visible_personas(user.id, None)
            .into_iter()
            .map(|p| Card {
                id: p.id,
                public_id: p.public_id.clone(),
                title: p.title.clone(),
                description: p.description.clone(),
                model: String::new(),
                paused: false,
                enabled: true,
            })
            .collect();
        v.sort_by(|a, b| a.title.cmp(&b.title));
        v
    };
    html(ListPage { nav: Nav::new(&user, "personas"), admin: user.is_admin(), cards })
}

struct ModelChoice {
    id: Uuid,
    public_id: String,
    selected: bool,
    off: bool,
}

struct ModelGroup {
    connection: String,
    models: Vec<ModelChoice>,
}

/// Every model, grouped by connection, for the dropdown.
async fn model_groups(st: &AppState, selected: Option<Uuid>) -> Result<Vec<ModelGroup>, WebError> {
    let conns = crate::connections::list(&st.pool).await?;
    let mut by_conn: BTreeMap<Uuid, Vec<ModelChoice>> = BTreeMap::new();
    for m in crate::connections::models(&st.pool).await? {
        by_conn.entry(m.connection_id).or_default().push(ModelChoice {
            selected: Some(m.id) == selected,
            off: !m.enabled,
            id: m.id,
            public_id: m.public_id,
        });
    }
    Ok(conns
        .into_iter()
        .filter_map(|c| {
            let models = by_conn.remove(&c.id)?;
            let connection = if c.enabled { c.name } else { format!("{} (disabled)", c.name) };
            Some(ModelGroup { connection, models })
        })
        .collect())
}

struct HistoryView {
    id: i64,
    at: String,
    by: String,
    text: String,
    /// The button, when the change can be undone.
    undo: Option<&'static str>,
    /// The prompt as it was before, for prompt edits.
    before: Option<String>,
}

fn history_view(h: store::HistoryRow, tz: chrono_tz::Tz) -> HistoryView {
    let name = |n: Option<String>| n.unwrap_or_else(|| "(removed)".into());
    let (text, undo, before) = match h.what.as_str() {
        "created" => (format!("Created on {}", name(h.to_model_name)), None, None),
        "model" => (
            format!("Model {} to {}", name(h.from_model_name), name(h.to_model_name)),
            h.from_model.map(|_| "Switch back"),
            None,
        ),
        "prompt" => {
            let mode = h.prev_mode.unwrap_or_default();
            (
                "Prompt edited".to_string(),
                Some("Restore the earlier prompt"),
                Some(format!("{mode}: {}", h.prev_prompt.unwrap_or_default())),
            )
        }
        _ => (
            format!("Settings changed, before: {}", Params::from_json(&h.prev_params.unwrap_or(Value::Null)).summary()),
            Some("Restore those settings"),
            None,
        ),
    };
    HistoryView {
        id: h.id,
        at: h.at.with_timezone(&tz).format("%Y-%m-%d %H:%M").to_string(),
        by: h.by_name,
        text,
        undo,
        before,
    }
}

#[derive(Template)]
#[template(path = "persona_form.html")]
struct FormPage {
    nav: Nav,
    /// None: a new persona.
    id: Option<Uuid>,
    public_id: String,
    title: String,
    description: String,
    groups: Vec<ModelGroup>,
    prompt: String,
    replace: bool,
    prompt_visible: bool,
    enabled: bool,
    params: Vec<ParamRow>,
    /// Its model or connection is off.
    paused: bool,
    history: Vec<HistoryView>,
}

pub async fn new_form(State(st): State<AppState>, AdminUser(me): AdminUser) -> Page {
    html(FormPage {
        nav: Nav::new(&me, "personas"),
        id: None,
        public_id: String::new(),
        title: String::new(),
        description: String::new(),
        groups: model_groups(&st, None).await?,
        prompt: String::new(),
        replace: false,
        prompt_visible: true,
        enabled: true,
        params: param_rows(&Params::default()),
        paused: false,
        history: vec![],
    })
}

#[derive(Template)]
#[template(path = "persona_view.html")]
struct ViewPage {
    nav: Nav,
    public_id: String,
    title: String,
    description: String,
    /// None: the admin keeps the prompt to themselves.
    prompt: Option<String>,
}

/// Admins get the editor; users with access get the persona's card, and
/// everyone else the same 404 as a persona that does not exist.
pub async fn show(State(st): State<AppState>, user: WebUser, Path(id): Path<Uuid>) -> Page {
    let p = store::get(&st.pool, id).await?.ok_or(ApiError::NotFound)?;
    if !user.is_admin() {
        let snap = st.gateway.load();
        let live = snap.personas.get(&p.public_id).is_some_and(|e| e.id == id);
        if !live || !snap.persona_allowed(user.id, id) {
            return Err(ApiError::NotFound.into());
        }
        return html(ViewPage {
            nav: Nav::new(&user, "personas"),
            prompt: p.prompt_visible.then_some(p.system_prompt),
            public_id: p.public_id,
            title: p.title,
            description: p.description,
        });
    }
    let history = store::history(&st.pool, id).await?.into_iter().map(|h| history_view(h, user.tz)).collect();
    html(FormPage {
        nav: Nav::new(&user, "personas"),
        id: Some(p.id),
        groups: model_groups(&st, Some(p.model_id)).await?,
        replace: PromptMode::parse(&p.prompt_mode) == PromptMode::Replace,
        params: param_rows(&Params::from_json(&p.params)),
        paused: !p.model_live,
        prompt_visible: p.prompt_visible,
        enabled: p.enabled,
        public_id: p.public_id,
        title: p.title,
        description: p.description,
        prompt: p.system_prompt,
        history,
    })
}

/// The persona form, parsed by hand: the parameter table has a pair of
/// fields per parameter.
fn input(body: &Bytes) -> anyhow::Result<PersonaInput> {
    let f: BTreeMap<String, String> = form_urlencoded::parse(body).into_owned().collect();
    let get = |k: &str| f.get(k).map(String::as_str).unwrap_or("");
    let public_id = get("public_id").trim().to_string();
    converge_core::names::check_public_id(&public_id).map_err(|e| anyhow::anyhow!("`{public_id}`: {e}"))?;
    Ok(PersonaInput {
        public_id,
        title: store::check_title(get("title"))?,
        description: store::check_description(get("description"))?,
        model_id: get("model_id").parse().map_err(|_| anyhow::anyhow!("pick a model for the persona"))?,
        system_prompt: store::check_prompt(get("system_prompt"))?,
        prompt_mode: PromptMode::parse(get("prompt_mode")),
        prompt_visible: f.contains_key("prompt_visible"),
        params: Params::from_form(|k| f.get(k).map(String::as_str), true).map_err(|e| anyhow::anyhow!(e))?,
        enabled: f.contains_key("enabled"),
    })
}

fn by(me: &WebUser) -> By<'_> {
    By { id: me.id, name: &me.email }
}

pub async fn create(State(st): State<AppState>, AdminUser(me): AdminUser, body: Bytes) -> Page {
    let input = input(&body).map_err(bad)?;
    let id = store::create(&st.pool, by(&me), &input).await.map_err(bad)?;
    st.reload().await?;
    Ok(Redirect::to(&format!("/personas/{id}")).into_response())
}

pub async fn update(State(st): State<AppState>, AdminUser(me): AdminUser, Path(id): Path<Uuid>, body: Bytes) -> Page {
    let input = input(&body).map_err(bad)?;
    store::update(&st.pool, by(&me), id, &input).await.map_err(bad)?;
    st.reload().await?;
    Ok(Redirect::to(&format!("/personas/{id}")).into_response())
}

pub async fn delete(State(st): State<AppState>, _a: AdminUser, Path(id): Path<Uuid>) -> Page {
    store::delete(&st.pool, id).await.map_err(bad)?;
    st.reload().await?;
    Ok(Redirect::to("/personas").into_response())
}

pub async fn restore(
    State(st): State<AppState>,
    AdminUser(me): AdminUser,
    Path((id, entry)): Path<(Uuid, i64)>,
) -> Page {
    store::restore(&st.pool, by(&me), id, entry).await.map_err(bad)?;
    st.reload().await?;
    Ok(Redirect::to(&format!("/personas/{id}")).into_response())
}
