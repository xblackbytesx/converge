//! Web login: Argon2id passwords and cookie sessions.
//!
//! Session cookies are random 256-bit values stored only as SHA-256 (like API
//! keys), `HttpOnly`, `SameSite=Strict`, `Secure` unless configured off.
//! Cross-site writes are additionally refused by the Origin check in `web`.

use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use axum::extract::FromRequestParts;
use axum::http::header::COOKIE;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{Duration, Utc};
use converge_core::secrets::SESSION_PREFIX;
use sqlx::PgPool;
use uuid::Uuid;

use crate::AppState;
use crate::auth::{Role, new_secret, secret_hash};

pub const COOKIE_NAME: &str = "converge_session";
const SESSION_DAYS: i64 = 30;

pub fn hash_password(password: &str) -> String {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default().hash_password(password.as_bytes(), &salt).expect("argon2 hashing").to_string()
}

pub fn verify_password(password: &str, stored: &str) -> bool {
    PasswordHash::new(stored).is_ok_and(|h| Argon2::default().verify_password(password.as_bytes(), &h).is_ok())
}

/// Checks a login; returns the user id. Unknown, disabled and password-less
/// users still pay one Argon2 verification, so timing does not reveal which
/// emails exist.
pub async fn check_login(pool: &PgPool, email: &str, password: &str) -> Result<Option<Uuid>, sqlx::Error> {
    let row: Option<(Uuid, Option<String>)> =
        sqlx::query_as("SELECT id, password_hash FROM users WHERE email = $1 AND disabled_at IS NULL")
            .bind(email.trim().to_lowercase())
            .fetch_optional(pool)
            .await?;
    match row {
        Some((id, Some(hash))) => Ok(verify_password(password, &hash).then_some(id)),
        _ => {
            let _ = verify_password(password, DUMMY_HASH);
            Ok(None)
        }
    }
}

/// A valid Argon2id hash of a random string, for constant-ish login timing.
const DUMMY_HASH: &str =
    "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzb21lc2FsdA$9sTbSlTio3Biev89thdrlKKiCaYsjjYVJxGAL3swxpQ";

/// Creates a session; returns the `Set-Cookie` value.
pub async fn create(pool: &PgPool, user: Uuid, secure: bool) -> Result<String, sqlx::Error> {
    let token = new_secret(SESSION_PREFIX);
    sqlx::query("INSERT INTO web_sessions (token_sha256, user_id, expires_at) VALUES ($1, $2, $3)")
        .bind(secret_hash(&token))
        .bind(user)
        .bind(Utc::now() + Duration::days(SESSION_DAYS))
        .execute(pool)
        .await?;
    // Opportunistic cleanup keeps the table small without a scheduler.
    sqlx::query("DELETE FROM web_sessions WHERE expires_at < now()").execute(pool).await?;
    Ok(cookie(&token, SESSION_DAYS * 86_400, secure))
}

pub async fn destroy(pool: &PgPool, token: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM web_sessions WHERE token_sha256 = $1").bind(secret_hash(token)).execute(pool).await?;
    Ok(())
}

pub fn cookie(value: &str, max_age: i64, secure: bool) -> String {
    format!(
        "{COOKIE_NAME}={value}; Path=/; Max-Age={max_age}; HttpOnly; SameSite=Strict{}",
        if secure { "; Secure" } else { "" }
    )
}

pub fn session_token(parts: &Parts) -> Option<String> {
    parts
        .headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == COOKIE_NAME)
        .map(|(_, v)| v.to_string())
        .filter(|v| converge_core::secrets::is_shaped(v, SESSION_PREFIX))
}

/// A logged-in browser user. Missing or expired sessions go to /login.
#[derive(Debug, Clone)]
pub struct WebUser {
    pub id: Uuid,
    pub email: String,
    pub name: String,
    pub role: Role,
    pub tz: chrono_tz::Tz,
    pub token: String,
}

impl WebUser {
    pub fn is_admin(&self) -> bool {
        self.role == Role::Admin
    }

    /// Display name, else the email.
    pub fn label(&self) -> &str {
        if self.name.is_empty() { &self.email } else { &self.name }
    }
}

pub struct ToLogin {
    htmx: bool,
}

impl IntoResponse for ToLogin {
    fn into_response(self) -> Response {
        if self.htmx {
            // A redirect would be followed by the XHR and swapped into a
            // fragment; tell HTMX to navigate the whole page instead.
            ([("hx-redirect", "/login")], "").into_response()
        } else {
            Redirect::to("/login").into_response()
        }
    }
}

impl FromRequestParts<AppState> for WebUser {
    type Rejection = ToLogin;

    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Self::Rejection> {
        let htmx = parts.headers.contains_key("hx-request");
        let token = session_token(parts).ok_or(ToLogin { htmx })?;
        let row: Option<(Uuid, String, String, String, String)> = sqlx::query_as(
            "SELECT u.id, u.email, u.display_name, u.role, u.tz FROM web_sessions s JOIN users u ON u.id = s.user_id
              WHERE s.token_sha256 = $1 AND s.expires_at > now() AND u.disabled_at IS NULL",
        )
        .bind(secret_hash(&token))
        .fetch_optional(&st.pool)
        .await
        .map_err(|_| ToLogin { htmx })?;
        let (id, email, name, role, tz) = row.ok_or(ToLogin { htmx })?;
        Ok(WebUser { id, email, name, role: Role::parse(&role), tz: tz.parse().unwrap_or(chrono_tz::UTC), token })
    }
}

/// A logged-in admin. Anyone else gets 403.
pub struct AdminUser(pub WebUser);

pub enum AdminRejection {
    Login(ToLogin),
    Forbidden,
}

impl IntoResponse for AdminRejection {
    fn into_response(self) -> Response {
        match self {
            AdminRejection::Login(l) => l.into_response(),
            AdminRejection::Forbidden => {
                crate::error::WebError(crate::error::ApiError::Forbidden("only an admin can do that".into()))
                    .into_response()
            }
        }
    }
}

impl FromRequestParts<AppState> for AdminUser {
    type Rejection = AdminRejection;

    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Self::Rejection> {
        let u = WebUser::from_request_parts(parts, st).await.map_err(AdminRejection::Login)?;
        if u.is_admin() { Ok(AdminUser(u)) } else { Err(AdminRejection::Forbidden) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_hash_and_verify() {
        let h = hash_password("correct horse");
        assert!(h.starts_with("$argon2id$"));
        assert!(verify_password("correct horse", &h));
        assert!(!verify_password("wrong", &h));
        assert!(!verify_password("x", "not a hash"));
        assert!(PasswordHash::new(DUMMY_HASH).is_ok(), "the timing dummy must parse");
    }

    #[test]
    fn cookie_flags() {
        assert_eq!(
            cookie("abc", 60, true),
            "converge_session=abc; Path=/; Max-Age=60; HttpOnly; SameSite=Strict; Secure"
        );
        assert!(!cookie("abc", 60, false).contains("Secure"));
    }
}
