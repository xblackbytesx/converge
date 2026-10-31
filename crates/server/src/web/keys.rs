//! API keys (everyone: their own; admins: everyone's), with optional scopes,
//! and per-person access and limits (admins). Writes reload the snapshot.

use std::collections::{BTreeMap, HashSet};

use askama::Template;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::session::{AdminUser, WebUser};
use super::{Nav, html};
use crate::AppState;
use crate::error::{ApiError, WebError};
use crate::{connections, keys};

type Page = Result<Response, WebError>;

struct KeyView {
    id: Uuid,
    name: String,
    prefix: String,
    owner: String,
    created: String,
    expires: String,
    last_used: String,
    revoked: bool,
    expired: bool,
    /// None: everything its owner may use.
    scope: Option<String>,
}

/// A model or persona on the key forms.
struct Pick {
    id: Uuid,
    public_id: String,
    checked: bool,
}

/// What a person may use, as checklists, ticked where `scope` names it.
fn picks(st: &AppState, principal: Uuid, scope: Option<&keys::KeyScope>) -> (Vec<Pick>, Vec<Pick>) {
    let snap = st.gateway.load();
    let has = |ids: Option<&Vec<Uuid>>, id: Uuid| ids.is_some_and(|v| v.contains(&id));
    let models = snap
        .visible(principal, None)
        .into_iter()
        .map(|m| Pick { id: m.id, public_id: m.public_id.clone(), checked: has(scope.map(|s| &s.models), m.id) })
        .collect();
    let personas = snap
        .visible_personas(principal, None)
        .into_iter()
        .map(|p| Pick { id: p.id, public_id: p.public_id.clone(), checked: has(scope.map(|s| &s.personas), p.id) })
        .collect();
    (models, personas)
}

/// The fields both key forms share: `name`, `days` (or `keep`), `reach`
/// (`all` or `some`) and any number of `model` and `persona` fields.
struct KeyForm {
    name: String,
    days: Option<String>,
    /// `reach=some`: only the ticked ones. Anything else: everything.
    scope: Option<keys::KeyScope>,
}

fn key_form(body: &[u8]) -> Result<KeyForm, ApiError> {
    let (mut name, mut days, mut some) = (String::new(), None, false);
    let mut scope = keys::KeyScope::default();
    for (k, v) in form_urlencoded::parse(body) {
        match k.as_ref() {
            "name" => name = v.into_owned(),
            "days" => days = Some(v.into_owned()),
            "reach" => some = v == "some",
            "model" => scope.models.push(v.parse().map_err(|_| ApiError::bad("unknown model"))?),
            "persona" => scope.personas.push(v.parse().map_err(|_| ApiError::bad("unknown persona"))?),
            _ => {}
        }
    }
    Ok(KeyForm { name, days, scope: some.then_some(scope) })
}

fn days(v: Option<&str>) -> Result<i64, ApiError> {
    v.and_then(|d| d.parse().ok()).ok_or_else(|| ApiError::bad("pick one of the offered expiry choices"))
}

#[derive(Template)]
#[template(path = "keys.html")]
struct KeysPage {
    nav: Nav,
    mine: Vec<KeyView>,
    /// Admins: everyone else's keys.
    others: Vec<KeyView>,
    expiry: &'static [i64],
    models: Vec<Pick>,
    personas: Vec<Pick>,
}

pub(super) fn when(t: Option<DateTime<Utc>>, tz: chrono_tz::Tz, none: &str) -> String {
    t.map(|t| t.with_timezone(&tz).format("%Y-%m-%d %H:%M").to_string()).unwrap_or_else(|| none.to_string())
}

fn view(k: keys::KeyRow, tz: chrono_tz::Tz) -> KeyView {
    KeyView {
        id: k.id,
        name: k.name,
        prefix: k.prefix,
        owner: k.owner,
        created: super::local_day(k.created_at, tz),
        expired: k.expires_at.is_some_and(|t| t <= Utc::now()),
        expires: when(k.expires_at, tz, "never"),
        last_used: when(k.last_used_at, tz, "never"),
        revoked: k.revoked_at.is_some(),
        scope: k.scoped.then(|| k.scope.unwrap_or_else(|| "nothing left".into())),
    }
}

pub async fn page(State(st): State<AppState>, user: WebUser) -> Page {
    let all = keys::list(&st.pool, if user.is_admin() { None } else { Some(user.id) }).await?;
    let (mut mine, mut others) = (vec![], vec![]);
    for k in all {
        if k.owner == user.email { mine.push(view(k, user.tz)) } else { others.push(view(k, user.tz)) }
    }
    let (models, personas) = picks(&st, user.id, None);
    html(KeysPage { nav: Nav::new(&user, "keys"), mine, others, expiry: keys::EXPIRY_DAYS, models, personas })
}

#[derive(Template)]
#[template(path = "key_created.html")]
struct KeyCreated {
    name: String,
    key: String,
    base: String,
}

/// The key form (see `key_form`); "everything" unless `reach=some`.
pub async fn create(State(st): State<AppState>, user: WebUser, headers: HeaderMap, body: Bytes) -> Page {
    let f = key_form(&body)?;
    let days = days(f.days.as_deref())?;
    let (_, key) = keys::create(&st.pool, user.id, &f.name, days, f.scope.as_ref())
        .await
        .map_err(|e| ApiError::bad(e.to_string()))?;
    st.reload().await?;
    html(KeyCreated { name: f.name.trim().to_string(), key, base: format!("{}/v1", st.public_base(&headers)) })
}

#[derive(Template)]
#[template(path = "key_edit.html")]
struct KeyEditPage {
    nav: Nav,
    id: Uuid,
    name: String,
    prefix: String,
    /// Empty for your own key; the owner's email for an admin.
    owner: String,
    expires: String,
    expiry: &'static [i64],
    /// The key reaches only the ticked ones.
    some: bool,
    models: Vec<Pick>,
    personas: Vec<Pick>,
}

/// Own live keys for everyone; any live key for admins. The checklist is
/// the key owner's, also when an admin edits it.
pub async fn edit_page(State(st): State<AppState>, user: WebUser, Path(id): Path<Uuid>) -> Page {
    let owner = (!user.is_admin()).then_some(user.id);
    let k = keys::get(&st.pool, id, owner).await.map_err(ApiError::Internal)?.ok_or(ApiError::NotFound)?;
    let (models, personas) = picks(&st, k.principal, k.scope.as_ref());
    html(KeyEditPage {
        nav: Nav::new(&user, "keys"),
        id: k.id,
        name: k.name,
        prefix: k.prefix,
        owner: if k.principal == user.id { String::new() } else { k.owner },
        expires: when(k.expires_at, user.tz, "never"),
        expiry: keys::EXPIRY_DAYS,
        some: k.scope.is_some(),
        models,
        personas,
    })
}

/// The key form with `days` either `keep` or one of the offered choices.
pub async fn save(State(st): State<AppState>, user: WebUser, Path(id): Path<Uuid>, body: Bytes) -> Page {
    let owner = (!user.is_admin()).then_some(user.id);
    let f = key_form(&body)?;
    let expiry = match f.days.as_deref() {
        None | Some("keep") => keys::Expiry::Keep,
        d => keys::Expiry::Days(days(d)?),
    };
    let found = keys::update(&st.pool, id, owner, &f.name, f.scope.as_ref(), expiry)
        .await
        .map_err(|e| ApiError::bad(e.to_string()))?;
    if !found {
        return Err(ApiError::NotFound.into());
    }
    st.reload().await?;
    Ok(Redirect::to("/keys").into_response())
}

/// Own keys for everyone; any key for admins.
pub async fn revoke(State(st): State<AppState>, user: WebUser, Path(id): Path<Uuid>) -> Page {
    let owner = (!user.is_admin()).then_some(user.id);
    if !keys::revoke(&st.pool, id, owner).await.map_err(ApiError::Internal)? {
        return Err(ApiError::NotFound.into());
    }
    st.reload().await?;
    Ok(Redirect::to("/keys").into_response())
}

struct AccessRow {
    id: Uuid,
    public_id: String,
    connection: String,
    granted: bool,
}

struct PersonaAccess {
    id: Uuid,
    public_id: String,
    title: String,
    granted: bool,
}

#[derive(Template)]
#[template(path = "access.html")]
struct AccessPage {
    nav: Nav,
    id: Uuid,
    who: String,
    admin: bool,
    all: bool,
    models: Vec<AccessRow>,
    personas: Vec<PersonaAccess>,
    rpm: String,
    tokens_per_day: String,
    parallel_per_key: String,
}

fn blank(v: Option<impl ToString>) -> String {
    v.map(|n| n.to_string()).unwrap_or_default()
}

#[derive(sqlx::FromRow)]
struct Person {
    email: String,
    role: String,
    rpm: Option<i32>,
    tokens_per_day: Option<i64>,
    parallel_per_key: Option<i32>,
}

pub async fn access_page(State(st): State<AppState>, AdminUser(me): AdminUser, Path(id): Path<Uuid>) -> Page {
    let who: Option<Person> =
        sqlx::query_as("SELECT email, role, rpm, tokens_per_day, parallel_per_key FROM users WHERE id = $1")
            .bind(id)
            .fetch_optional(&st.pool)
            .await?;
    let Person { email: who, role, rpm, tokens_per_day, parallel_per_key } = who.ok_or(ApiError::NotFound)?;
    let g = keys::grants(&st.pool, id).await?;
    let granted: HashSet<Uuid> = g.models.into_iter().chain(g.personas).collect();
    let names: BTreeMap<Uuid, String> =
        connections::list(&st.pool).await?.into_iter().map(|c| (c.id, c.name)).collect();
    let models = connections::models(&st.pool)
        .await?
        .into_iter()
        .map(|m| AccessRow {
            granted: granted.contains(&m.id),
            connection: names.get(&m.connection_id).cloned().unwrap_or_default(),
            id: m.id,
            public_id: m.public_id,
        })
        .collect();
    let personas = crate::personas::list(&st.pool)
        .await?
        .into_iter()
        .map(|p| PersonaAccess { granted: granted.contains(&p.id), id: p.id, public_id: p.public_id, title: p.title })
        .collect();
    html(AccessPage {
        nav: Nav::new(&me, "admin"),
        id,
        who,
        admin: role == "admin",
        all: g.all,
        models,
        personas,
        rpm: blank(rpm),
        tokens_per_day: blank(tokens_per_day),
        parallel_per_key: blank(parallel_per_key),
    })
}

/// Empty: no limit. Otherwise a whole number of at least 1.
fn limit<T: std::str::FromStr + PartialOrd + From<u8>>(v: &str, what: &str) -> Result<Option<T>, ApiError> {
    let v = v.trim().replace([' ', '_', '\u{a0}'], "");
    if v.is_empty() {
        return Ok(None);
    }
    match v.parse::<T>() {
        Ok(n) if n >= T::from(1) => Ok(Some(n)),
        _ => Err(ApiError::bad(format!("{what}: a whole number of at least 1, or empty for no limit"))),
    }
}

/// `all` (checkbox), any number of `model` and `persona` fields, and the
/// three limits.
pub async fn save_access(State(st): State<AppState>, _a: AdminUser, Path(id): Path<Uuid>, body: Bytes) -> Page {
    let mut all = false;
    let (mut models, mut personas) = (Vec::new(), Vec::new());
    let (mut rpm, mut tokens, mut parallel) = (None, None, None);
    for (k, v) in form_urlencoded::parse(&body) {
        match k.as_ref() {
            "all" => all = true,
            "model" => models.push(v.parse::<Uuid>().map_err(|_| ApiError::bad("unknown model"))?),
            "persona" => personas.push(v.parse::<Uuid>().map_err(|_| ApiError::bad("unknown persona"))?),
            "rpm" => rpm = limit::<i32>(&v, "Requests per minute")?,
            "tokens_per_day" => tokens = limit::<i64>(&v, "Tokens per day")?,
            "parallel_per_key" => parallel = limit::<i32>(&v, "Parallel requests per key")?,
            _ => {}
        }
    }
    keys::set_grants(&st.pool, id, all, &models, &personas).await.map_err(ApiError::Internal)?;
    sqlx::query("UPDATE users SET rpm = $2, tokens_per_day = $3, parallel_per_key = $4 WHERE id = $1")
        .bind(id)
        .bind(rpm)
        .bind(tokens)
        .bind(parallel)
        .execute(&st.pool)
        .await?;
    st.reload().await?;
    Ok(Redirect::to(&format!("/admin/people/{id}/access")).into_response())
}
