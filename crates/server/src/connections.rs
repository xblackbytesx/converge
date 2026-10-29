//! Layer 1: connections and their models. Validation, storage with sealed
//! credentials, and the two calls to the upstream the website makes ("test"
//! and "import"), both a plain `GET {base_url}/models`.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::http::HeaderName;
use converge_core::names::check_public_id;
use converge_core::persona::Params;
use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::crypto::Secrets;
use crate::snapshot::{credential_aad, headers_aad};

pub use converge_core::persona::TOKEN_FIELDS;

/// Headers Converge sets itself or that would break the request; an extra
/// header may not replace them.
const RESERVED: &[&str] = &[
    "authorization",
    "x-api-key",
    "anthropic-version",
    "host",
    "content-length",
    "content-type",
    "transfer-encoding",
    "connection",
    "cookie",
    "accept",
];

/// What an upstream speaks. Declared by the admin, never inferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    OpenAi,
    Anthropic,
}

impl Kind {
    pub fn parse(s: &str) -> Self {
        if s == "anthropic" { Kind::Anthropic } else { Kind::OpenAi }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::OpenAi => "openai",
            Kind::Anthropic => "anthropic",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Responses {
    Native,
    Translate,
}

impl Responses {
    pub fn parse(s: &str) -> Self {
        if s == "native" { Responses::Native } else { Responses::Translate }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Responses::Native => "native",
            Responses::Translate => "translate",
        }
    }
}

/// What the connection form submits, already checked.
pub struct ConnInput {
    pub kind: Kind,
    pub name: String,
    pub slug: String,
    pub base_url: String,
    /// None: keep what is stored. Some(""): no credential.
    pub credential: Option<String>,
    /// None: keep. Some(empty): none.
    pub headers: Option<Vec<(String, String)>>,
    pub responses: Responses,
    /// `max_tokens` or `max_completion_tokens` (see `0003_personas.sql`).
    pub token_field: String,
    pub first_byte_secs: i32,
    pub chunk_secs: i32,
    pub enabled: bool,
}

pub fn check_token_field(f: &str) -> Result<String> {
    if !TOKEN_FIELDS.contains(&f) {
        bail!("the token limit field is max_tokens or max_completion_tokens");
    }
    Ok(f.to_string())
}

pub fn check_name(name: &str) -> Result<String> {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > 60 {
        bail!("give the connection a name of at most 60 characters");
    }
    Ok(n.to_string())
}

/// Slugs prefix imported model ids, so they follow the public id rules and
/// contain no `/` of their own.
pub fn check_slug(slug: &str) -> Result<String> {
    let s = slug.trim().to_lowercase();
    if s.contains('/') {
        bail!("the slug cannot contain /");
    }
    check_public_id(&s).map_err(|e| anyhow::anyhow!("slug: {e}"))?;
    Ok(s)
}

/// `http(s)://host[:port]/path`, no query or fragment, no trailing slash.
pub fn check_base_url(url: &str) -> Result<String> {
    let u = url.trim().trim_end_matches('/');
    let rest = u.strip_prefix("https://").or_else(|| u.strip_prefix("http://"));
    let Some(rest) = rest else { bail!("the base URL must start with https:// (or http:// on a LAN)") };
    if rest.is_empty() || rest.starts_with('/') {
        bail!("the base URL needs a host");
    }
    if u.contains(['?', '#', ' ']) || rest.contains('@') {
        bail!("the base URL cannot have a query, fragment, spaces or user info");
    }
    Ok(u.to_string())
}

/// `Name: value` per line; blank lines ignored.
pub fn parse_header_lines(text: &str) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let Some((name, value)) = line.split_once(':') else { bail!("header lines look like `Name: value`") };
        let name = name.trim().to_lowercase();
        let value = value.trim().to_string();
        if HeaderName::from_bytes(name.as_bytes()).is_err() || name.is_empty() {
            bail!("`{name}` is not a valid header name");
        }
        if RESERVED.contains(&name.as_str()) {
            bail!("`{name}` is set by Converge itself and cannot be an extra header");
        }
        if axum::http::HeaderValue::from_str(&value).is_err() {
            bail!("the value of `{name}` has characters a header cannot carry");
        }
        out.push((name, value));
    }
    Ok(out)
}

pub fn check_secs(v: i32, what: &str) -> Result<i32> {
    if !(1..=3600).contains(&v) {
        bail!("{what} must be between 1 and 3600 seconds");
    }
    Ok(v)
}

fn seal_headers(secrets: &Secrets, id: Uuid, h: &[(String, String)]) -> Option<Vec<u8>> {
    (!h.is_empty()).then(|| secrets.seal(&serde_json::to_string(h).expect("pairs serialize"), &headers_aad(id)))
}

fn seal_credential(secrets: &Secrets, id: Uuid, c: &str) -> Option<Vec<u8>> {
    let c = c.trim();
    (!c.is_empty()).then(|| secrets.seal(c, &credential_aad(id)))
}

fn unique_hint(e: sqlx::Error) -> anyhow::Error {
    match &e {
        sqlx::Error::Database(d) if d.is_unique_violation() => anyhow::anyhow!("that name or slug is already in use"),
        _ => e.into(),
    }
}

pub async fn create(pool: &PgPool, secrets: &Secrets, input: &ConnInput) -> Result<Uuid> {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO connections (id, name, slug, base_url, credential, extra_headers, responses, first_byte_secs, chunk_secs, enabled, token_field, kind)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(id)
    .bind(&input.name)
    .bind(&input.slug)
    .bind(&input.base_url)
    .bind(input.credential.as_deref().and_then(|c| seal_credential(secrets, id, c)))
    .bind(input.headers.as_deref().and_then(|h| seal_headers(secrets, id, h)))
    .bind(input.responses.as_str())
    .bind(input.first_byte_secs)
    .bind(input.chunk_secs)
    .bind(input.enabled)
    .bind(&input.token_field)
    .bind(input.kind.as_str())
    .execute(pool)
    .await
    .map_err(unique_hint)?;
    Ok(id)
}

pub async fn update(pool: &PgPool, secrets: &Secrets, id: Uuid, input: &ConnInput) -> Result<()> {
    let mut tx = pool.begin().await?;
    let done = sqlx::query(
        "UPDATE connections SET name = $2, slug = $3, base_url = $4, responses = $5, first_byte_secs = $6, chunk_secs = $7, enabled = $8,
                token_field = $9, kind = $10
          WHERE id = $1",
    )
    .bind(id)
    .bind(&input.name)
    .bind(&input.slug)
    .bind(&input.base_url)
    .bind(input.responses.as_str())
    .bind(input.first_byte_secs)
    .bind(input.chunk_secs)
    .bind(input.enabled)
    .bind(&input.token_field)
    .bind(input.kind.as_str())
    .execute(&mut *tx)
    .await
    .map_err(unique_hint)?;
    if done.rows_affected() == 0 {
        bail!("no such connection");
    }
    if let Some(c) = &input.credential {
        sqlx::query("UPDATE connections SET credential = $2 WHERE id = $1")
            .bind(id)
            .bind(seal_credential(secrets, id, c))
            .execute(&mut *tx)
            .await?;
    }
    if let Some(h) = &input.headers {
        sqlx::query("UPDATE connections SET extra_headers = $2 WHERE id = $1")
            .bind(id)
            .bind(seal_headers(secrets, id, h))
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Removes a connection, its models and every grant on those models.
/// Refused while a persona runs on one of them.
pub async fn delete(pool: &PgPool, id: Uuid) -> Result<()> {
    let mut tx = pool.begin().await?;
    let users: Vec<String> = sqlx::query_scalar(
        "SELECT p.public_id FROM personas p JOIN models m ON m.id = p.model_id WHERE m.connection_id = $1 ORDER BY 1",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    in_use(&users)?;
    for table in ["grants", "key_scopes"] {
        sqlx::query(&format!(
            "DELETE FROM {table} WHERE target_kind = 'model' AND target_id IN (SELECT id FROM models WHERE connection_id = $1)"
        ))
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("DELETE FROM connections WHERE id = $1").bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// A connection as the edit form shows it: secrets only as "set or not".
#[derive(sqlx::FromRow)]
pub struct ConnRow {
    pub id: Uuid,
    pub kind: String,
    pub name: String,
    pub slug: String,
    pub base_url: String,
    pub has_credential: bool,
    pub responses: String,
    pub token_field: String,
    pub first_byte_secs: i32,
    pub chunk_secs: i32,
    pub enabled: bool,
}

pub async fn get(pool: &PgPool, id: Uuid) -> Result<Option<ConnRow>> {
    Ok(sqlx::query_as(
        "SELECT id, kind, name, slug, base_url, credential IS NOT NULL AS has_credential, responses, token_field, first_byte_secs, chunk_secs,
                enabled
           FROM connections WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

pub async fn list(pool: &PgPool) -> Result<Vec<ConnRow>> {
    Ok(sqlx::query_as(
        "SELECT id, kind, name, slug, base_url, credential IS NOT NULL AS has_credential, responses, token_field, first_byte_secs, chunk_secs,
                enabled
           FROM connections ORDER BY name",
    )
    .fetch_all(pool)
    .await?)
}

/// Names of the stored extra headers (values never leave the server).
pub async fn header_names(pool: &PgPool, secrets: &Secrets, id: Uuid) -> Result<Vec<String>> {
    let sealed: Option<Vec<u8>> = sqlx::query_scalar("SELECT extra_headers FROM connections WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?
        .flatten();
    let Some(sealed) = sealed else { return Ok(vec![]) };
    let json = secrets.open(&sealed, &headers_aad(id))?;
    let pairs: Vec<(String, String)> = serde_json::from_str(&json).unwrap_or_default();
    Ok(pairs.into_iter().map(|(n, _)| n).collect())
}

#[derive(Deserialize)]
struct ModelList {
    data: Vec<ModelItem>,
    /// Anthropic's paging.
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    last_id: Option<String>,
}

#[derive(Deserialize)]
struct ModelItem {
    id: String,
}

/// What `GET {base_url}/models` says, with this connection's credential and
/// headers, in its kind's way (Anthropic: `x-api-key`, `anthropic-version`,
/// pages of up to 1000). Used by "Test connection" and "Import models".
pub async fn upstream_models(
    pool: &PgPool,
    secrets: &Secrets,
    http: &reqwest::Client,
    id: Uuid,
) -> Result<Vec<String>> {
    type Sealed = Option<Vec<u8>>;
    let row: Option<(String, Sealed, Sealed, String)> =
        sqlx::query_as("SELECT base_url, credential, extra_headers, kind FROM connections WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    let Some((base, cred, headers, kind)) = row else { bail!("no such connection") };
    let anthropic = Kind::parse(&kind) == Kind::Anthropic;
    let cred = cred.map(|c| secrets.open(&c, &credential_aad(id))).transpose()?;
    let pairs: Vec<(String, String)> = match headers {
        Some(h) => serde_json::from_str(&secrets.open(&h, &headers_aad(id))?)?,
        None => vec![],
    };
    let mut ids = Vec::new();
    let mut after: Option<String> = None;
    // Anthropic pages; ten pages of 1000 is more than any account has.
    for _ in 0..10 {
        let url = match (&after, anthropic) {
            (_, false) => format!("{base}/models"),
            (None, true) => format!("{base}/models?limit=1000"),
            (Some(a), true) => format!("{base}/models?limit=1000&after_id={a}"),
        };
        let mut req = http.get(url).timeout(Duration::from_secs(15));
        if let Some(c) = &cred {
            req = if anthropic { req.header("x-api-key", c) } else { req.bearer_auth(c) };
        }
        if anthropic {
            req = req.header("anthropic-version", converge_core::anthropic::VERSION);
        }
        for (n, v) in &pairs {
            req = req.header(n, v);
        }
        let resp = req.send().await.map_err(|e| anyhow::anyhow!("could not reach the upstream: {}", short(&e)))?;
        let status = resp.status();
        if !status.is_success() {
            bail!("the upstream answered {status}");
        }
        let bytes = resp.bytes().await.context("reading the model list")?;
        let list: ModelList =
            serde_json::from_slice(&bytes).context("the upstream's /models answer is not a model list")?;
        ids.extend(list.data.into_iter().map(|m| m.id));
        match (anthropic && list.has_more, list.last_id) {
            (true, Some(last)) => after = Some(last),
            _ => break,
        }
    }
    ids.sort();
    Ok(ids)
}

/// A reqwest error without the URL it carries (it may hold a token in a
/// path for some providers).
fn short(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "timed out"
    } else if e.is_connect() {
        "connection refused or unreachable"
    } else {
        "request failed"
    }
}

/// The public id an import suggests for an upstream model: `{slug}/{name}`,
/// lowercased, anything a public id cannot hold turned into `-`.
pub fn suggest_public_id(slug: &str, upstream: &str) -> String {
    let mut s = String::new();
    for c in upstream.to_lowercase().chars() {
        let ok = c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-' | '/');
        let c = if ok { c } else { '-' };
        let repeated = matches!(c, '-' | '/') && s.ends_with(c);
        if !repeated {
            s.push(c);
        }
    }
    let s = s.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    format!("{slug}/{s}")
}

/// Takes the public id namespace for the rest of the transaction. Models
/// and personas share it across two tables, so a unique index cannot
/// guard it; this lock makes check-then-insert safe against a second admin.
pub async fn lock_public_ids(tx: &mut sqlx::PgConnection) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('converge.public_ids'))").execute(tx).await?;
    Ok(())
}

/// Is a public id free in both tables? `except` is the row being renamed.
/// Call under [`lock_public_ids`].
pub async fn public_id_free(tx: &mut sqlx::PgConnection, public_id: &str, except: Option<Uuid>) -> Result<bool> {
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM models WHERE public_id = $1 AND id IS DISTINCT FROM $2)
             OR EXISTS (SELECT 1 FROM personas WHERE public_id = $1 AND id IS DISTINCT FROM $2)",
    )
    .bind(public_id)
    .bind(except)
    .fetch_one(tx)
    .await?;
    Ok(!taken)
}

/// Checks a new or renamed public id: shape, then free (under the lock).
pub async fn claim_public_id(tx: &mut sqlx::PgConnection, public_id: &str, except: Option<Uuid>) -> Result<()> {
    check_public_id(public_id).map_err(|e| anyhow::anyhow!("`{public_id}`: {e}"))?;
    lock_public_ids(&mut *tx).await?;
    if !public_id_free(tx, public_id, except).await? {
        bail!("`{public_id}` is already in use by a model or persona");
    }
    Ok(())
}

/// Adds models in one transaction: all of them or none.
pub async fn add_models(pool: &PgPool, connection: Uuid, picks: &[(String, String)]) -> Result<()> {
    let mut tx = pool.begin().await?;
    for (public_id, upstream_name) in picks {
        insert_model(&mut tx, connection, public_id, upstream_name).await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn add_model(pool: &PgPool, connection: Uuid, public_id: &str, upstream_name: &str) -> Result<Uuid> {
    let mut tx = pool.begin().await?;
    let id = insert_model(&mut tx, connection, public_id, upstream_name).await?;
    tx.commit().await?;
    Ok(id)
}

async fn insert_model(
    tx: &mut sqlx::PgConnection,
    connection: Uuid,
    public_id: &str,
    upstream_name: &str,
) -> Result<Uuid> {
    let upstream = upstream_name.trim();
    if upstream.is_empty() || upstream.len() > 200 {
        bail!("the upstream model name must be 1 to 200 characters");
    }
    claim_public_id(&mut *tx, public_id, None).await?;
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO models (id, public_id, connection_id, upstream_name) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(public_id)
        .bind(connection)
        .bind(upstream)
        .execute(tx)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(d) if d.is_unique_violation() => {
                anyhow::anyhow!("`{public_id}` or that upstream model is already added")
            }
            _ => e.into(),
        })?;
    Ok(id)
}

/// The switches on a model row: enabled; reasoning (items in translated
/// Responses answers, summarized thinking on Anthropic); prompt caching
/// (Anthropic).
pub async fn set_model_switches(
    pool: &PgPool,
    id: Uuid,
    enabled: bool,
    reasoning_items: bool,
    prompt_cache: bool,
) -> Result<()> {
    sqlx::query("UPDATE models SET enabled = $2, reasoning_items = $3, prompt_cache = $4 WHERE id = $1")
        .bind(id)
        .bind(enabled)
        .bind(reasoning_items)
        .bind(prompt_cache)
        .execute(pool)
        .await?;
    Ok(())
}

/// A model's default parameters (`core::persona::Params`, defaults only).
pub async fn set_model_params(pool: &PgPool, id: Uuid, params: &Params) -> Result<()> {
    sqlx::query("UPDATE models SET params = $2 WHERE id = $1").bind(id).bind(params.to_json()).execute(pool).await?;
    Ok(())
}

pub async fn rename_model(pool: &PgPool, id: Uuid, public_id: &str) -> Result<()> {
    let mut tx = pool.begin().await?;
    claim_public_id(&mut tx, public_id, Some(id)).await?;
    sqlx::query("UPDATE models SET public_id = $2 WHERE id = $1").bind(id).bind(public_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// Personas running on a model, by public id.
pub async fn personas_on(pool: &PgPool, model: Uuid) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar("SELECT public_id FROM personas WHERE model_id = $1 ORDER BY public_id")
        .bind(model)
        .fetch_all(pool)
        .await?)
}

fn in_use(personas: &[String]) -> Result<()> {
    if !personas.is_empty() {
        bail!(
            "in use by the persona{} {}: move them to another model first",
            if personas.len() == 1 { "" } else { "s" },
            personas.join(", ")
        );
    }
    Ok(())
}

/// A model and its grants, in one transaction. Refused while a persona runs
/// on it (and the database refuses too: `ON DELETE RESTRICT`).
pub async fn delete_model(pool: &PgPool, id: Uuid) -> Result<()> {
    let mut tx = pool.begin().await?;
    let users: Vec<String> = sqlx::query_scalar("SELECT public_id FROM personas WHERE model_id = $1 ORDER BY 1")
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
    in_use(&users)?;
    for table in ["grants", "key_scopes"] {
        sqlx::query(&format!("DELETE FROM {table} WHERE target_kind = 'model' AND target_id = $1"))
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("DELETE FROM models WHERE id = $1").bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

#[derive(sqlx::FromRow)]
pub struct ModelRow {
    pub id: Uuid,
    pub public_id: String,
    pub connection_id: Uuid,
    pub upstream_name: String,
    pub enabled: bool,
    pub params: serde_json::Value,
    pub reasoning_items: bool,
    pub prompt_cache: bool,
}

pub async fn models(pool: &PgPool) -> Result<Vec<ModelRow>> {
    Ok(sqlx::query_as(
        "SELECT id, public_id, connection_id, upstream_name, enabled, params, reasoning_items, prompt_cache
           FROM models ORDER BY public_id",
    )
    .fetch_all(pool)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_urls() {
        assert_eq!(check_base_url(" https://api.openai.com/v1/ ").unwrap(), "https://api.openai.com/v1");
        assert_eq!(check_base_url("http://llama-swap:8080/v1").unwrap(), "http://llama-swap:8080/v1");
        for bad in
            ["api.openai.com/v1", "ftp://x/v1", "https://", "https:///v1", "https://x/v1?k=1", "https://u:p@x/v1"]
        {
            assert!(check_base_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn header_lines() {
        let h = parse_header_lines("HTTP-Referer: https://me.example\n\n X-Title :  Converge \n").unwrap();
        assert_eq!(
            h,
            vec![("http-referer".into(), "https://me.example".into()), ("x-title".into(), "Converge".into())]
        );
        assert!(parse_header_lines("Authorization: Bearer x").is_err(), "reserved");
        assert!(parse_header_lines("no colon here").is_err());
        assert!(parse_header_lines("bad name: x").is_err());
        assert!(parse_header_lines("x-a: line\u{7f}").is_err());
        assert_eq!(parse_header_lines("").unwrap(), vec![]);
    }

    #[test]
    fn slugs_and_suggestions() {
        assert_eq!(check_slug(" Local ").unwrap(), "local");
        assert!(check_slug("a/b").is_err());
        assert!(check_slug("").is_err());
        assert_eq!(suggest_public_id("local", "Qwen3-30B-A3B"), "local/qwen3-30b-a3b");
        assert_eq!(suggest_public_id("gemini", "models/gemini-2.5-pro"), "gemini/models/gemini-2.5-pro");
        assert_eq!(suggest_public_id("or", "meta-llama/llama-4:free"), "or/meta-llama/llama-4-free");
        assert_eq!(suggest_public_id("x", "Weird  Name (Q4)"), "x/weird-name-q4");
        for (slug, up) in [("local", "Qwen3-30B-A3B"), ("or", "meta-llama/llama-4:free"), ("x", "Weird  Name (Q4)")] {
            assert_eq!(check_public_id(&suggest_public_id(slug, up)), Ok(()), "{up}");
        }
    }
}
