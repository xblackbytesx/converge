//! Accounts and invites. Every user is also a principal (same id), which is
//! what keys and grants will point at.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use converge_core::secrets::INVITE_PREFIX;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::auth::{self, Role};

pub const MIN_PASSWORD: usize = 10;
pub const INVITE_DAYS: i64 = 7;

pub fn normalize_email(email: &str) -> Result<String> {
    let e = email.trim().to_lowercase();
    // Not RFC parsing: just enough to catch a typo before it becomes a login.
    let ok = e.len() <= 254
        && e.split_once('@').is_some_and(|(l, d)| !l.is_empty() && d.contains('.') && !d.starts_with('.'))
        && !e.chars().any(char::is_whitespace);
    if !ok {
        bail!("that is not an email address");
    }
    Ok(e)
}

pub fn check_password(password: &str) -> Result<()> {
    if password.chars().count() < MIN_PASSWORD {
        bail!("use at least {MIN_PASSWORD} characters");
    }
    if password.len() > 1024 {
        bail!("that password is too long");
    }
    Ok(())
}

/// The principal row a user's id points at. Same transaction as the user.
async fn insert_principal(tx: &mut Transaction<'_, Postgres>, id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO principals (id, kind) VALUES ($1, 'user')").bind(id).execute(&mut **tx).await?;
    Ok(())
}

pub async fn create(pool: &PgPool, email: &str, role: Role) -> Result<Uuid> {
    let email = normalize_email(email)?;
    let id = Uuid::new_v4();
    let mut tx = pool.begin().await?;
    insert_principal(&mut tx, id).await?;
    sqlx::query("INSERT INTO users (id, email, role) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(&email)
        .bind(role.as_str())
        .execute(&mut *tx)
        .await
        .context("create user (does the email already exist?)")?;
    tx.commit().await?;
    Ok(id)
}

/// Sets (or replaces) the web login password. Ends every web session.
pub async fn set_password(pool: &PgPool, email: &str, password: &str) -> Result<()> {
    check_password(password)?;
    let hash = crate::web::session::hash_password(password);
    let id: Option<Uuid> = sqlx::query_scalar("UPDATE users SET password_hash = $2 WHERE email = $1 RETURNING id")
        .bind(email.trim().to_lowercase())
        .bind(hash)
        .fetch_optional(pool)
        .await?;
    let Some(id) = id else { bail!("no user with email {email}") };
    sqlx::query("DELETE FROM web_sessions WHERE user_id = $1").bind(id).execute(pool).await?;
    Ok(())
}

/// Creates a one-time invite; returns the plaintext token for the link.
pub async fn create_invite(pool: &PgPool, by: Uuid, role: Role, note: &str) -> Result<(String, DateTime<Utc>)> {
    let token = auth::new_secret(INVITE_PREFIX);
    let expires = Utc::now() + Duration::days(INVITE_DAYS);
    sqlx::query(
        "INSERT INTO invites (id, token_sha256, role, note, created_by, expires_at) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(Uuid::new_v4())
    .bind(auth::secret_hash(&token))
    .bind(role.as_str())
    .bind(note.trim().chars().take(80).collect::<String>())
    .bind(by)
    .bind(expires)
    .execute(pool)
    .await?;
    Ok((token, expires))
}

/// The role an invite grants, when it is still usable.
pub async fn invite_role(pool: &PgPool, token: &str) -> Result<Option<Role>> {
    if !converge_core::secrets::is_shaped(token, INVITE_PREFIX) {
        return Ok(None);
    }
    let role: Option<String> = sqlx::query_scalar(
        "SELECT role FROM invites WHERE token_sha256 = $1 AND used_at IS NULL AND expires_at > now()",
    )
    .bind(auth::secret_hash(token))
    .fetch_optional(pool)
    .await?;
    Ok(role.map(|r| Role::parse(&r)))
}

pub enum AcceptError {
    Invalid,
    Input(String),
}

/// Redeems an invite: claims it and creates the account in one transaction,
/// so two people racing on one link cannot both get in.
pub async fn accept_invite(
    pool: &PgPool,
    token: &str,
    email: &str,
    display_name: &str,
    password: &str,
) -> Result<Uuid, AcceptError> {
    let email = normalize_email(email).map_err(|e| AcceptError::Input(e.to_string()))?;
    check_password(password).map_err(|e| AcceptError::Input(e.to_string()))?;
    let name: String = display_name.trim().chars().take(60).collect();
    let hash = crate::web::session::hash_password(password);
    let mut tx = pool.begin().await.map_err(|_| AcceptError::Invalid)?;
    let invite: Option<(Uuid, String)> = sqlx::query_as(
        "UPDATE invites SET used_at = now()
          WHERE token_sha256 = $1 AND used_at IS NULL AND expires_at > now()
          RETURNING id, role",
    )
    .bind(auth::secret_hash(token))
    .fetch_optional(&mut *tx)
    .await
    .map_err(|_| AcceptError::Invalid)?;
    let Some((invite_id, role)) = invite else { return Err(AcceptError::Invalid) };
    let taken: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users WHERE email = $1)")
        .bind(&email)
        .fetch_one(&mut *tx)
        .await
        .map_err(|_| AcceptError::Invalid)?;
    if taken {
        // Dropping the transaction rolls back, so the invite stays usable
        // with another address.
        return Err(AcceptError::Input("an account with that email already exists".into()));
    }
    let id = Uuid::new_v4();
    insert_principal(&mut tx, id).await.map_err(|_| AcceptError::Invalid)?;
    sqlx::query("INSERT INTO users (id, email, display_name, role, password_hash) VALUES ($1, $2, $3, $4, $5)")
        .bind(id)
        .bind(&email)
        .bind(&name)
        .bind(&role)
        .bind(hash)
        .execute(&mut *tx)
        .await
        // A racing sign-up with the same email lost to ours: the unique
        // index refuses it and everything rolls back.
        .map_err(|_| AcceptError::Input("an account with that email already exists".into()))?;
    sqlx::query("UPDATE invites SET used_by = $2 WHERE id = $1")
        .bind(invite_id)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|_| AcceptError::Invalid)?;
    tx.commit().await.map_err(|_| AcceptError::Invalid)?;
    Ok(id)
}

/// Active admins other than `except`. Guards against locking everyone out.
pub async fn other_admins(pool: &PgPool, except: Uuid) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM users WHERE role = 'admin' AND disabled_at IS NULL AND id <> $1")
        .bind(except)
        .fetch_one(pool)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emails_are_normalised_and_checked() {
        assert_eq!(normalize_email("  Me@Example.COM ").unwrap(), "me@example.com");
        for bad in ["me", "@example.com", "me@example", "me@.com", "m e@example.com"] {
            assert!(normalize_email(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn passwords_need_ten_characters() {
        assert!(check_password("123456789").is_err());
        assert!(check_password("1234567890").is_ok());
        assert!(check_password(&"x".repeat(1025)).is_err());
    }
}
