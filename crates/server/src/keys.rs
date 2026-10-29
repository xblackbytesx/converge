//! API keys and grants. Both belong to principals (a user now, a team
//! later); the request path reads them from the snapshot, never from here.

use anyhow::{Result, bail};
use chrono::{DateTime, Duration, Utc};
use converge_core::secrets::{KEY_PREFIX, display_prefix};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth;

/// Expiry choices on the form, in days. 0 means never.
pub const EXPIRY_DAYS: &[i64] = &[0, 7, 30, 90, 365];

/// A scoped key's models and personas. Only ids that exist are kept, and
/// at request time only those its owner may still use count.
#[derive(Debug, Default, Clone)]
pub struct KeyScope {
    pub models: Vec<Uuid>,
    pub personas: Vec<Uuid>,
}

/// Mints a key and returns it in plaintext (shown once) with its id.
/// `scope` None: everything its owner may use.
pub async fn create(
    pool: &PgPool,
    principal: Uuid,
    name: &str,
    days: i64,
    scope: Option<&KeyScope>,
) -> Result<(Uuid, String)> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 60 {
        bail!("give the key a name of at most 60 characters");
    }
    if !EXPIRY_DAYS.contains(&days) {
        bail!("pick one of the offered expiry choices");
    }
    let key = auth::new_secret(KEY_PREFIX);
    let id = Uuid::new_v4();
    let expires = (days > 0).then(|| Utc::now() + Duration::days(days));
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO api_keys (id, principal_id, name, key_sha256, prefix, expires_at, scoped)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(principal)
    .bind(name)
    .bind(auth::secret_hash(&key))
    .bind(display_prefix(&key))
    .bind(expires)
    .bind(scope.is_some())
    .execute(&mut *tx)
    .await?;
    if let Some(s) = scope {
        let m = sqlx::query(
            "INSERT INTO key_scopes (key_id, target_kind, target_id) SELECT $1, 'model', id FROM models WHERE id = ANY($2)",
        )
        .bind(id)
        .bind(&s.models)
        .execute(&mut *tx)
        .await?;
        let p = sqlx::query(
            "INSERT INTO key_scopes (key_id, target_kind, target_id)
             SELECT $1, 'persona', id FROM personas WHERE id = ANY($2)",
        )
        .bind(id)
        .bind(&s.personas)
        .execute(&mut *tx)
        .await?;
        if m.rows_affected() + p.rows_affected() == 0 {
            bail!("pick at least one model or persona for this key, or none for everything");
        }
    }
    tx.commit().await?;
    Ok((id, key))
}

/// Revokes a key. `owner` limits it to that principal's keys (None: admin).
pub async fn revoke(pool: &PgPool, id: Uuid, owner: Option<Uuid>) -> Result<bool> {
    let r = sqlx::query(
        "UPDATE api_keys SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL AND ($2::uuid IS NULL OR principal_id = $2)",
    )
    .bind(id)
    .bind(owner)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() == 1)
}

#[derive(sqlx::FromRow)]
pub struct KeyRow {
    pub id: Uuid,
    pub name: String,
    pub prefix: String,
    pub owner: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub scoped: bool,
    /// A scoped key's models and personas by public id; None when all of
    /// them are gone.
    pub scope: Option<String>,
}

/// Keys of one principal, or everyone's (None), newest first.
pub async fn list(pool: &PgPool, principal: Option<Uuid>) -> Result<Vec<KeyRow>> {
    Ok(sqlx::query_as(
        "SELECT k.id, k.name, k.prefix, u.email AS owner, k.created_at, k.expires_at, k.revoked_at, k.last_used_at,
                k.scoped,
                (SELECT string_agg(coalesce(m.public_id, p.public_id), ', ' ORDER BY coalesce(m.public_id, p.public_id))
                   FROM key_scopes s
                   LEFT JOIN models m ON s.target_kind = 'model' AND m.id = s.target_id
                   LEFT JOIN personas p ON s.target_kind = 'persona' AND p.id = s.target_id
                  WHERE s.key_id = k.id) AS scope
           FROM api_keys k JOIN users u ON u.id = k.principal_id
          WHERE $1::uuid IS NULL OR k.principal_id = $1
          ORDER BY k.revoked_at IS NOT NULL, k.created_at DESC",
    )
    .bind(principal)
    .fetch_all(pool)
    .await?)
}

/// A principal's grants: everything, or these models and personas.
pub struct Grants {
    pub all: bool,
    pub models: Vec<Uuid>,
    pub personas: Vec<Uuid>,
}

pub async fn grants(pool: &PgPool, principal: Uuid) -> Result<Grants> {
    let rows: Vec<(String, Option<Uuid>)> =
        sqlx::query_as("SELECT target_kind, target_id FROM grants WHERE principal_id = $1")
            .bind(principal)
            .fetch_all(pool)
            .await?;
    Ok(Grants {
        all: rows.iter().any(|(k, _)| k == "all"),
        models: rows.iter().filter(|(k, _)| k == "model").filter_map(|(_, t)| *t).collect(),
        personas: rows.iter().filter(|(k, _)| k == "persona").filter_map(|(_, t)| *t).collect(),
    })
}

/// Replaces all of a principal's grants.
pub async fn set_grants(pool: &PgPool, principal: Uuid, all: bool, models: &[Uuid], personas: &[Uuid]) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM grants WHERE principal_id = $1").bind(principal).execute(&mut *tx).await?;
    if all {
        sqlx::query("INSERT INTO grants (principal_id, target_kind) VALUES ($1, 'all')")
            .bind(principal)
            .execute(&mut *tx)
            .await?;
    }
    // Only ids that are models: a stale form cannot grant something else.
    sqlx::query(
        "INSERT INTO grants (principal_id, target_kind, target_id)
         SELECT $1, 'model', id FROM models WHERE id = ANY($2)",
    )
    .bind(principal)
    .bind(models)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO grants (principal_id, target_kind, target_id)
         SELECT $1, 'persona', id FROM personas WHERE id = ANY($2)",
    )
    .bind(principal)
    .bind(personas)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}
