//! Connections and their models (layer 1). Admins manage them here; users
//! see the models their grants allow on the same address, titled Models.
//! Every write reloads the snapshot before answering.

use std::collections::{BTreeMap, HashSet};

use askama::Template;
use axum::Form;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;
use uuid::Uuid;

use super::personas::{ParamRow, param_rows};
use super::session::{AdminUser, WebUser};
use super::{Nav, html};
use crate::AppState;
use crate::connections::{self as store, ConnInput, ConnRow, Kind, Responses};
use crate::error::{ApiError, WebError};
use converge_core::persona::Params;

type Page = Result<Response, WebError>;

fn bad(e: anyhow::Error) -> WebError {
    ApiError::bad(e.to_string()).into()
}

struct ModelView {
    id: Uuid,
    public_id: String,
    upstream_name: String,
    enabled: bool,
    reasoning_items: bool,
    prompt_cache: bool,
    /// Personas running on it (removing it is refused while there are any).
    personas: Vec<(Uuid, String)>,
    defaults: String,
    params: Vec<ParamRow>,
}

struct ConnView {
    c: ConnRow,
    models: Vec<ModelView>,
}

#[derive(Template)]
#[template(path = "connections.html")]
struct ConnectionsPage {
    nav: Nav,
    conns: Vec<ConnView>,
}

impl ConnView {
    fn anthropic(&self) -> bool {
        self.c.kind == "anthropic"
    }
}

struct UserModel {
    public_id: String,
    connection: String,
}

#[derive(Template)]
#[template(path = "models_user.html")]
struct ModelsPage {
    nav: Nav,
    models: Vec<UserModel>,
}

pub async fn page(State(st): State<AppState>, user: WebUser) -> Page {
    if !user.is_admin() {
        let snap = st.gateway.load();
        let models = snap
            .visible(user.id, None)
            .into_iter()
            .map(|m| UserModel { public_id: m.public_id.clone(), connection: m.connection.name.clone() })
            .collect();
        return html(ModelsPage { nav: Nav::new(&user, "connections"), models });
    }
    let mut on_model: BTreeMap<Uuid, Vec<(Uuid, String)>> = BTreeMap::new();
    for p in crate::personas::list(&st.pool).await? {
        on_model.entry(p.model_id).or_default().push((p.id, p.public_id));
    }
    let mut by_conn: BTreeMap<Uuid, Vec<ModelView>> = BTreeMap::new();
    for m in store::models(&st.pool).await? {
        let params = Params::from_json(&m.params);
        by_conn.entry(m.connection_id).or_default().push(ModelView {
            id: m.id,
            public_id: m.public_id,
            upstream_name: m.upstream_name,
            enabled: m.enabled,
            reasoning_items: m.reasoning_items,
            prompt_cache: m.prompt_cache,
            personas: on_model.remove(&m.id).unwrap_or_default(),
            defaults: params.summary(),
            params: param_rows(&params),
        });
    }
    let conns = store::list(&st.pool)
        .await?
        .into_iter()
        .map(|c| ConnView { models: by_conn.remove(&c.id).unwrap_or_default(), c })
        .collect();
    html(ConnectionsPage { nav: Nav::new(&user, "connections"), conns })
}

#[derive(Template)]
#[template(path = "connection_form.html")]
struct FormPage {
    nav: Nav,
    /// None: a new connection.
    id: Option<Uuid>,
    anthropic: bool,
    name: String,
    slug: String,
    base_url: String,
    has_credential: bool,
    header_names: Vec<String>,
    native_responses: bool,
    completion_tokens: bool,
    first_byte_secs: i32,
    chunk_secs: i32,
    enabled: bool,
}

pub async fn new_form(AdminUser(me): AdminUser) -> Page {
    html(FormPage {
        nav: Nav::new(&me, "connections"),
        id: None,
        anthropic: false,
        name: String::new(),
        slug: String::new(),
        base_url: String::new(),
        has_credential: false,
        header_names: vec![],
        native_responses: false,
        completion_tokens: false,
        first_byte_secs: 600,
        chunk_secs: 120,
        enabled: true,
    })
}

pub async fn edit_form(State(st): State<AppState>, AdminUser(me): AdminUser, Path(id): Path<Uuid>) -> Page {
    let c = store::get(&st.pool, id).await?.ok_or(ApiError::NotFound)?;
    // An unreadable header blob shows as none; saving new ones replaces it.
    let header_names = store::header_names(&st.pool, &st.secrets, id).await.unwrap_or_default();
    html(FormPage {
        nav: Nav::new(&me, "connections"),
        id: Some(c.id),
        anthropic: c.kind == "anthropic",
        name: c.name,
        slug: c.slug,
        base_url: c.base_url,
        has_credential: c.has_credential,
        header_names,
        native_responses: c.responses == "native",
        completion_tokens: c.token_field == "max_completion_tokens",
        first_byte_secs: c.first_byte_secs,
        chunk_secs: c.chunk_secs,
        enabled: c.enabled,
    })
}

#[derive(Deserialize)]
pub struct ConnForm {
    #[serde(default)]
    kind: String,
    name: String,
    slug: String,
    base_url: String,
    #[serde(default)]
    credential: String,
    clear_credential: Option<String>,
    #[serde(default)]
    headers: String,
    clear_headers: Option<String>,
    responses: String,
    #[serde(default = "default_token_field")]
    token_field: String,
    first_byte_secs: i32,
    chunk_secs: i32,
    enabled: Option<String>,
}

fn default_token_field() -> String {
    "max_tokens".into()
}

impl ConnForm {
    /// `editing`: blank credential and headers keep what is stored.
    fn input(&self, editing: bool) -> anyhow::Result<ConnInput> {
        let credential = if self.clear_credential.is_some() {
            Some(String::new())
        } else if self.credential.trim().is_empty() && editing {
            None
        } else {
            Some(self.credential.clone())
        };
        let headers = if self.clear_headers.is_some() {
            Some(vec![])
        } else if self.headers.trim().is_empty() && editing {
            None
        } else {
            Some(store::parse_header_lines(&self.headers)?)
        };
        let kind = Kind::parse(&self.kind);
        Ok(ConnInput {
            kind,
            name: store::check_name(&self.name)?,
            slug: store::check_slug(&self.slug)?,
            base_url: store::check_base_url(&self.base_url)?,
            credential,
            headers,
            // Anthropic has no Responses API of its own.
            responses: if kind == Kind::Anthropic { Responses::Translate } else { Responses::parse(&self.responses) },
            token_field: store::check_token_field(&self.token_field)?,
            first_byte_secs: store::check_secs(self.first_byte_secs, "time to first byte")?,
            chunk_secs: store::check_secs(self.chunk_secs, "time between chunks")?,
            enabled: self.enabled.is_some(),
        })
    }
}

pub async fn create(State(st): State<AppState>, _a: AdminUser, Form(f): Form<ConnForm>) -> Page {
    let input = f.input(false).map_err(bad)?;
    let id = store::create(&st.pool, &st.secrets, &input).await.map_err(bad)?;
    st.reload().await?;
    Ok(Redirect::to(&format!("/connections/{id}/import")).into_response())
}

pub async fn update(State(st): State<AppState>, _a: AdminUser, Path(id): Path<Uuid>, Form(f): Form<ConnForm>) -> Page {
    let input = f.input(true).map_err(bad)?;
    store::update(&st.pool, &st.secrets, id, &input).await.map_err(bad)?;
    st.reload().await?;
    Ok(Redirect::to("/connections").into_response())
}

pub async fn delete(State(st): State<AppState>, _a: AdminUser, Path(id): Path<Uuid>) -> Page {
    store::delete(&st.pool, id).await.map_err(bad)?;
    st.reload().await?;
    Ok(Redirect::to("/connections").into_response())
}

#[derive(Template)]
#[template(path = "connection_test.html")]
struct TestResult {
    ok: bool,
    message: String,
}

/// "Test connection": the upstream's model list, summarised. An HTMX
/// fragment; failures are a result to show, not an error.
pub async fn test(State(st): State<AppState>, _a: AdminUser, Path(id): Path<Uuid>) -> Page {
    let r = match store::upstream_models(&st.pool, &st.secrets, &st.http, id).await {
        Ok(list) => TestResult { ok: true, message: format!("Reachable. It lists {} models.", list.len()) },
        Err(e) => TestResult { ok: false, message: e.to_string() },
    };
    html(r)
}

struct ImportRow {
    upstream: String,
    suggested: String,
    /// Already a model on this connection.
    added: bool,
}

#[derive(Template)]
#[template(path = "import.html")]
struct ImportPage {
    nav: Nav,
    id: Uuid,
    name: String,
    rows: Vec<ImportRow>,
    error: Option<String>,
}

pub async fn import_page(State(st): State<AppState>, AdminUser(me): AdminUser, Path(id): Path<Uuid>) -> Page {
    let c = store::get(&st.pool, id).await?.ok_or(ApiError::NotFound)?;
    let have: HashSet<String> =
        store::models(&st.pool).await?.into_iter().filter(|m| m.connection_id == id).map(|m| m.upstream_name).collect();
    let (rows, error) = match store::upstream_models(&st.pool, &st.secrets, &st.http, id).await {
        Ok(list) => (
            list.into_iter()
                .map(|u| ImportRow {
                    suggested: store::suggest_public_id(&c.slug, &u),
                    added: have.contains(&u),
                    upstream: u,
                })
                .collect(),
            None,
        ),
        Err(e) => (vec![], Some(e.to_string())),
    };
    html(ImportPage { nav: Nav::new(&me, "connections"), id, name: c.name, rows, error })
}

/// The import form: rows `n`, each with `up_n`, `pub_n` and, when ticked,
/// `on_n`. Parsed by hand because axum's `Form` cannot collect repeated
/// fields. Adds every ticked row or none, and says which one failed.
pub async fn import(State(st): State<AppState>, _a: AdminUser, Path(id): Path<Uuid>, body: Bytes) -> Page {
    let fields: BTreeMap<String, String> = form_urlencoded::parse(&body).into_owned().collect();
    let mut picks = Vec::new();
    for (k, _) in fields.iter().filter(|(k, _)| k.starts_with("on_")) {
        let n = &k[3..];
        let (Some(up), Some(public)) = (fields.get(&format!("up_{n}")), fields.get(&format!("pub_{n}"))) else {
            continue;
        };
        picks.push((up.clone(), public.trim().to_string()));
    }
    if picks.is_empty() {
        return Err(ApiError::bad("tick at least one model").into());
    }
    let mut seen = HashSet::new();
    for (_, public) in &picks {
        if !seen.insert(public.clone()) {
            return Err(ApiError::bad(format!("`{public}` is used twice")).into());
        }
    }
    let picks: Vec<(String, String)> = picks.into_iter().map(|(up, public)| (public, up)).collect();
    store::add_models(&st.pool, id, &picks).await.map_err(bad)?;
    st.reload().await?;
    Ok(Redirect::to("/connections").into_response())
}

#[derive(Deserialize)]
pub struct AddModelForm {
    public_id: String,
    upstream_name: String,
}

/// A model the upstream does not list (or not yet): by hand.
pub async fn add_model(
    State(st): State<AppState>,
    _a: AdminUser,
    Path(id): Path<Uuid>,
    Form(f): Form<AddModelForm>,
) -> Page {
    store::add_model(&st.pool, id, f.public_id.trim(), &f.upstream_name).await.map_err(bad)?;
    st.reload().await?;
    Ok(Redirect::to("/connections").into_response())
}

#[derive(Deserialize)]
pub struct ModelForm {
    enabled: Option<String>,
    reasoning: Option<String>,
    cache: Option<String>,
    public_id: Option<String>,
}

/// One model row: rename and both switches in one form.
pub async fn save_model(
    State(st): State<AppState>,
    _a: AdminUser,
    Path(id): Path<Uuid>,
    Form(f): Form<ModelForm>,
) -> Page {
    if let Some(p) = f.public_id.as_deref().map(str::trim) {
        store::rename_model(&st.pool, id, p).await.map_err(bad)?;
    }
    store::set_model_switches(&st.pool, id, f.enabled.is_some(), f.reasoning.is_some(), f.cache.is_some())
        .await
        .map_err(bad)?;
    st.reload().await?;
    Ok(Redirect::to("/connections").into_response())
}

pub async fn delete_model(State(st): State<AppState>, _a: AdminUser, Path(id): Path<Uuid>) -> Page {
    store::delete_model(&st.pool, id).await.map_err(bad)?;
    st.reload().await?;
    Ok(Redirect::to("/connections").into_response())
}

/// A model's default parameters: `{key}_value` per parameter, blank is off.
pub async fn save_params(State(st): State<AppState>, _a: AdminUser, Path(id): Path<Uuid>, body: Bytes) -> Page {
    let fields: BTreeMap<String, String> = form_urlencoded::parse(&body).into_owned().collect();
    let params = Params::from_form(|k| fields.get(k).map(String::as_str), false).map_err(ApiError::bad)?;
    store::set_model_params(&st.pool, id, &params).await.map_err(bad)?;
    st.reload().await?;
    Ok(Redirect::to("/connections").into_response())
}
