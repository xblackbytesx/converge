//! What the request path reads: keys, connections, models, personas and
//! grants. Built
//! from Postgres, published behind `arc-swap`, so a request does one atomic
//! load and never touches the database or a lock (AGENTS.md decision 5).
//!
//! Every admin write calls [`Gateway::reload`]. Reloads run one at a time
//! under a mutex, so two writes can never publish out of order.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use arc_swap::ArcSwap;
use axum::http::{HeaderName, HeaderValue};
use chrono::{DateTime, Utc};
use converge_core::persona::{Params, PromptMode};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::connections::Kind;
use crate::crypto::Secrets;
use crate::limits::Limits;

/// Associated data for a connection's sealed columns: row and column.
pub fn credential_aad(id: Uuid) -> Vec<u8> {
    [id.as_bytes().as_slice(), b"credential"].concat()
}

pub fn headers_aad(id: Uuid) -> Vec<u8> {
    [id.as_bytes().as_slice(), b"extra_headers"].concat()
}

/// One HTTP client for every upstream: its pool keeps connections per host.
/// No overall timeout (streams run as long as they run) and no read timeout
/// (it would also cover waiting for headers and cut off a model load); the
/// proxy times first byte and every chunk itself. No redirects: one could
/// carry the credential to another host.
pub fn upstream_client() -> reqwest::Client {
    // reqwest is built without a default crypto provider (aws-lc needs cmake
    // everywhere); ring is installed once, here, before any client exists.
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_nodelay(true)
        .user_agent(concat!("converge/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("reqwest client")
}

#[derive(Debug, Clone)]
pub struct KeyEntry {
    pub id: Uuid,
    pub principal: Uuid,
    pub prefix: String,
    /// For the request log line.
    pub owner: String,
    pub expires_at: Option<DateTime<Utc>>,
    /// A scoped key: only these, and only while the owner has the grant.
    pub scope: Option<Arc<Scope>>,
}

/// What a scoped key may use (on top of its owner's grants).
#[derive(Debug, Default)]
pub struct Scope {
    pub models: HashSet<Uuid>,
    pub personas: HashSet<Uuid>,
}

pub struct ConnEntry {
    pub id: Uuid,
    pub kind: Kind,
    pub name: String,
    pub base_url: String,
    /// The credential as this kind sends it, value marked sensitive:
    /// `Authorization: Bearer ...` or Anthropic's `x-api-key`.
    pub auth: Option<(HeaderName, HeaderValue)>,
    pub extra_headers: Vec<(HeaderName, HeaderValue)>,
    /// The stored credential or headers could not be opened (secret key
    /// changed). Requests get a clear 502 instead of an anonymous call.
    pub unreadable: bool,
    pub first_byte: Duration,
    pub chunk: Duration,
    /// Where a token limit setting goes when the client sent none.
    pub token_field: &'static str,
    /// `/v1/responses` goes to the upstream's own; else translated to chat.
    pub native_responses: bool,
}

pub struct ModelEntry {
    pub id: Uuid,
    pub public_id: String,
    pub upstream_name: String,
    pub created: i64,
    pub connection: Arc<ConnEntry>,
    /// Defaults for every request to this model, persona or not.
    pub params: Params,
    /// Translated Responses answers carry the upstream's reasoning; on
    /// Anthropic also asks for summarized thinking.
    pub reasoning_items: bool,
    /// Anthropic prompt caching on every request.
    pub prompt_cache: bool,
}

/// An enabled persona whose model is live, with that model resolved, so a
/// request needs one lookup.
pub struct PersonaEntry {
    pub id: Uuid,
    pub public_id: String,
    pub title: String,
    pub description: String,
    pub created: i64,
    pub model: Arc<ModelEntry>,
    pub prompt: String,
    pub mode: PromptMode,
    pub params: Params,
}

#[derive(Default)]
pub struct Access {
    pub all: bool,
    pub models: HashSet<Uuid>,
    pub personas: HashSet<Uuid>,
}

#[derive(Default)]
pub struct Snapshot {
    pub generation: u64,
    pub keys: HashMap<Vec<u8>, KeyEntry>,
    pub models: HashMap<String, Arc<ModelEntry>>,
    pub personas: HashMap<String, Arc<PersonaEntry>>,
    pub access: HashMap<Uuid, Access>,
    /// Admins can use everything without grants.
    pub admins: HashSet<Uuid>,
    /// Per person; missing means no limits.
    pub limits: HashMap<Uuid, Limits>,
}

impl Snapshot {
    pub fn allowed(&self, principal: Uuid, model: Uuid) -> bool {
        self.admins.contains(&principal)
            || self.access.get(&principal).is_some_and(|a| a.all || a.models.contains(&model))
    }

    /// A grant on a persona is enough; its model needs none.
    pub fn persona_allowed(&self, principal: Uuid, persona: Uuid) -> bool {
        self.admins.contains(&principal)
            || self.access.get(&principal).is_some_and(|a| a.all || a.personas.contains(&persona))
    }

    /// The models a principal (through a key's scope, if any) may use,
    /// sorted by public id.
    pub fn visible(&self, principal: Uuid, scope: Option<&Scope>) -> Vec<&Arc<ModelEntry>> {
        let mut v: Vec<_> = self
            .models
            .values()
            .filter(|m| self.allowed(principal, m.id) && scope.is_none_or(|s| s.models.contains(&m.id)))
            .collect();
        v.sort_by(|a, b| a.public_id.cmp(&b.public_id));
        v
    }

    /// The personas a principal (through a key's scope, if any) may use,
    /// sorted by public id.
    pub fn visible_personas(&self, principal: Uuid, scope: Option<&Scope>) -> Vec<&Arc<PersonaEntry>> {
        let mut v: Vec<_> = self
            .personas
            .values()
            .filter(|p| self.persona_allowed(principal, p.id) && scope.is_none_or(|s| s.personas.contains(&p.id)))
            .collect();
        v.sort_by(|a, b| a.public_id.cmp(&b.public_id));
        v
    }
}

pub struct Gateway {
    current: ArcSwap<Snapshot>,
    reload_lock: tokio::sync::Mutex<u64>,
    secrets: Arc<Secrets>,
}

impl Gateway {
    pub fn new(secrets: Arc<Secrets>) -> Self {
        Self { current: ArcSwap::from_pointee(Snapshot::default()), reload_lock: Default::default(), secrets }
    }

    pub fn load(&self) -> Arc<Snapshot> {
        self.current.load_full()
    }

    /// Rebuilds the snapshot from Postgres and publishes it.
    pub async fn reload(&self, pool: &PgPool) -> Result<()> {
        let mut generation = self.reload_lock.lock().await;
        *generation += 1;
        let snap = build(pool, &self.secrets, *generation).await?;
        tracing::debug!(
            generation = snap.generation,
            keys = snap.keys.len(),
            models = snap.models.len(),
            personas = snap.personas.len(),
            "snapshot"
        );
        self.current.store(Arc::new(snap));
        Ok(())
    }
}

#[derive(sqlx::FromRow)]
struct KeyRow {
    id: Uuid,
    principal_id: Uuid,
    key_sha256: Vec<u8>,
    prefix: String,
    expires_at: Option<DateTime<Utc>>,
    email: String,
    scoped: bool,
}

#[derive(sqlx::FromRow)]
struct ConnRow {
    id: Uuid,
    kind: String,
    name: String,
    base_url: String,
    credential: Option<Vec<u8>>,
    extra_headers: Option<Vec<u8>>,
    first_byte_secs: i32,
    chunk_secs: i32,
    token_field: String,
    responses: String,
}

#[derive(sqlx::FromRow)]
struct ModelRow {
    id: Uuid,
    public_id: String,
    upstream_name: String,
    created_at: DateTime<Utc>,
    connection_id: Uuid,
    params: Value,
    reasoning_items: bool,
    prompt_cache: bool,
}

#[derive(sqlx::FromRow)]
struct PersonaRow {
    id: Uuid,
    public_id: String,
    title: String,
    description: String,
    created_at: DateTime<Utc>,
    model_id: Uuid,
    system_prompt: String,
    prompt_mode: String,
    params: Value,
}

#[derive(sqlx::FromRow)]
struct LimitRow {
    id: Uuid,
    rpm: Option<i32>,
    tokens_per_day: Option<i64>,
    parallel_per_key: Option<i32>,
    tz: String,
}

async fn build(pool: &PgPool, secrets: &Secrets, generation: u64) -> Result<Snapshot> {
    let mut snap = Snapshot { generation, ..Default::default() };

    let keys: Vec<KeyRow> = sqlx::query_as(
        "SELECT k.id, k.principal_id, k.key_sha256, k.prefix, k.expires_at, u.email, k.scoped
           FROM api_keys k JOIN users u ON u.id = k.principal_id
          WHERE k.revoked_at IS NULL AND u.disabled_at IS NULL AND (k.expires_at IS NULL OR k.expires_at > now())",
    )
    .fetch_all(pool)
    .await?;
    let mut scopes: HashMap<Uuid, Scope> = HashMap::new();
    let rows: Vec<(Uuid, String, Uuid)> =
        sqlx::query_as("SELECT key_id, target_kind, target_id FROM key_scopes").fetch_all(pool).await?;
    for (key, kind, target) in rows {
        let s = scopes.entry(key).or_default();
        if kind == "persona" {
            s.personas.insert(target);
        } else {
            s.models.insert(target);
        }
    }
    for k in keys {
        let scope = k.scoped.then(|| Arc::new(scopes.remove(&k.id).unwrap_or_default()));
        snap.keys.insert(
            k.key_sha256,
            KeyEntry {
                id: k.id,
                principal: k.principal_id,
                prefix: k.prefix,
                owner: k.email,
                expires_at: k.expires_at,
                scope,
            },
        );
    }

    let conns: Vec<ConnRow> = sqlx::query_as(
        "SELECT id, kind, name, base_url, credential, extra_headers, first_byte_secs, chunk_secs, token_field, responses
           FROM connections WHERE enabled",
    )
    .fetch_all(pool)
    .await?;
    let mut by_id = HashMap::new();
    for c in conns {
        let entry = open_connection(c, secrets);
        by_id.insert(entry.id, Arc::new(entry));
    }

    let models: Vec<ModelRow> = sqlx::query_as(
        "SELECT id, public_id, upstream_name, created_at, connection_id, params, reasoning_items, prompt_cache
           FROM models WHERE enabled",
    )
    .fetch_all(pool)
    .await?;
    let mut model_by_id = HashMap::new();
    for m in models {
        let Some(connection) = by_id.get(&m.connection_id).cloned() else { continue };
        let entry = Arc::new(ModelEntry {
            id: m.id,
            public_id: m.public_id.clone(),
            upstream_name: m.upstream_name,
            created: m.created_at.timestamp(),
            connection,
            params: Params::from_json(&m.params),
            reasoning_items: m.reasoning_items,
            prompt_cache: m.prompt_cache,
        });
        model_by_id.insert(m.id, entry.clone());
        snap.models.insert(m.public_id, entry);
    }

    // A persona whose model (or its connection) is off is left out: paused.
    let personas: Vec<PersonaRow> = sqlx::query_as(
        "SELECT id, public_id, title, description, created_at, model_id, system_prompt, prompt_mode, params
           FROM personas WHERE enabled",
    )
    .fetch_all(pool)
    .await?;
    for p in personas {
        let Some(model) = model_by_id.get(&p.model_id).cloned() else { continue };
        snap.personas.insert(
            p.public_id.clone(),
            Arc::new(PersonaEntry {
                id: p.id,
                public_id: p.public_id,
                title: p.title,
                description: p.description,
                created: p.created_at.timestamp(),
                model,
                prompt: p.system_prompt,
                mode: PromptMode::parse(&p.prompt_mode),
                params: Params::from_json(&p.params),
            }),
        );
    }

    let grants: Vec<(Uuid, String, Option<Uuid>)> =
        sqlx::query_as("SELECT principal_id, target_kind, target_id FROM grants").fetch_all(pool).await?;
    for (principal, kind, target) in grants {
        let a = snap.access.entry(principal).or_default();
        match (kind.as_str(), target) {
            ("all", _) => a.all = true,
            ("model", Some(t)) => {
                a.models.insert(t);
            }
            ("persona", Some(t)) => {
                a.personas.insert(t);
            }
            _ => {}
        }
    }

    snap.admins = sqlx::query_scalar("SELECT id FROM users WHERE role = 'admin' AND disabled_at IS NULL")
        .fetch_all(pool)
        .await?
        .into_iter()
        .collect();

    let limits: Vec<LimitRow> = sqlx::query_as(
        "SELECT id, rpm, tokens_per_day, parallel_per_key, tz FROM users
          WHERE disabled_at IS NULL AND (rpm IS NOT NULL OR tokens_per_day IS NOT NULL OR parallel_per_key IS NOT NULL)",
    )
    .fetch_all(pool)
    .await?;
    for l in limits {
        let positive = |v: Option<i32>| v.and_then(|n| u32::try_from(n).ok()).filter(|n| *n > 0);
        snap.limits.insert(
            l.id,
            Limits {
                rpm: positive(l.rpm),
                tokens_per_day: l.tokens_per_day.filter(|n| *n > 0),
                parallel_per_key: positive(l.parallel_per_key),
                tz: l.tz.parse().unwrap_or(chrono_tz::UTC),
            },
        );
    }
    Ok(snap)
}

fn open_connection(c: ConnRow, secrets: &Secrets) -> ConnEntry {
    let mut unreadable = false;
    let kind = Kind::parse(&c.kind);
    let auth = c.credential.as_deref().and_then(|sealed| match secrets.open(sealed, &credential_aad(c.id)) {
        Ok(cred) => {
            let (name, value) = match kind {
                Kind::OpenAi => (axum::http::header::AUTHORIZATION, format!("Bearer {cred}")),
                Kind::Anthropic => (HeaderName::from_static("x-api-key"), cred),
            };
            let mut v = HeaderValue::from_str(&value).ok()?;
            v.set_sensitive(true);
            Some((name, v))
        }
        Err(e) => {
            tracing::error!(connection = %c.name, "{e}");
            unreadable = true;
            None
        }
    });
    let extra_headers = match c.extra_headers.as_deref().map(|s| secrets.open(s, &headers_aad(c.id))) {
        None => vec![],
        Some(Ok(json)) => parse_headers(&json),
        Some(Err(e)) => {
            tracing::error!(connection = %c.name, "{e}");
            unreadable = true;
            vec![]
        }
    };
    ConnEntry {
        id: c.id,
        kind,
        name: c.name,
        base_url: c.base_url,
        auth,
        extra_headers,
        unreadable,
        first_byte: Duration::from_secs(c.first_byte_secs.max(1) as u64),
        chunk: Duration::from_secs(c.chunk_secs.max(1) as u64),
        token_field: if c.token_field == "max_completion_tokens" { "max_completion_tokens" } else { "max_tokens" },
        // Anthropic has no Responses API of its own: always translated.
        native_responses: kind == Kind::OpenAi && c.responses == "native",
    }
}

/// The stored JSON list of `[name, value]` pairs. Validated when saved, so
/// anything unparsable here is skipped rather than fatal.
fn parse_headers(json: &str) -> Vec<(HeaderName, HeaderValue)> {
    let pairs: Vec<(String, String)> = serde_json::from_str(json).unwrap_or_default();
    pairs
        .into_iter()
        .filter_map(|(n, v)| {
            let mut v = HeaderValue::from_str(&v).ok()?;
            v.set_sensitive(true);
            Some((HeaderName::from_bytes(n.as_bytes()).ok()?, v))
        })
        .collect()
}
