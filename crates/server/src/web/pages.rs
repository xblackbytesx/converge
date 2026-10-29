//! Login, invites and personal settings.

use std::time::Instant;

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::http::header::SET_COOKIE;
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use super::session::{self, WebUser};
use super::{ClientAddr, Nav, html};
use crate::AppState;
use crate::error::{ApiError, WebError};
use crate::users::{self, AcceptError};

type Page = Result<Response, WebError>;

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    email: String,
    error: Option<&'static str>,
}

pub async fn login_page() -> Page {
    html(LoginPage { email: String::new(), error: None })
}

#[derive(Deserialize)]
pub struct LoginForm {
    email: String,
    password: String,
}

pub async fn login(State(st): State<AppState>, ClientAddr(addr): ClientAddr, Form(f): Form<LoginForm>) -> Page {
    let again = |error, status| -> Page {
        let mut r = html(LoginPage { email: f.email.clone(), error: Some(error) })?;
        *r.status_mut() = status;
        Ok(r)
    };
    // Checked before Argon2 runs, so a blocked guesser costs no CPU.
    if !st.logins.allowed(addr, &f.email, Instant::now()) {
        return again("Too many failed attempts. Wait five minutes and try again.", StatusCode::TOO_MANY_REQUESTS);
    }
    match session::check_login(&st.pool, &f.email, &f.password).await? {
        Some(user) => {
            st.logins.succeeded(&f.email);
            let cookie = session::create(&st.pool, user, st.cookie_secure).await?;
            Ok(([(SET_COOKIE, cookie)], Redirect::to("/")).into_response())
        }
        None => {
            st.logins.failed(addr, &f.email, Instant::now());
            // Same answer for unknown email, no password set, wrong password.
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            again("Wrong email or password.", StatusCode::UNAUTHORIZED)
        }
    }
}

pub async fn logout(State(st): State<AppState>, user: WebUser) -> Page {
    session::destroy(&st.pool, &user.token).await?;
    Ok(([(SET_COOKIE, session::cookie("", 0, st.cookie_secure))], Redirect::to("/login")).into_response())
}

#[derive(Template)]
#[template(path = "invite.html")]
struct InvitePage {
    token: String,
    valid: bool,
    admin: bool,
    email: String,
    name: String,
    error: Option<String>,
}

pub async fn invite_page(State(st): State<AppState>, Path(token): Path<String>) -> Page {
    let role = users::invite_role(&st.pool, &token).await.map_err(ApiError::Internal)?;
    let mut r = html(InvitePage {
        valid: role.is_some(),
        admin: role == Some(crate::auth::Role::Admin),
        token,
        email: String::new(),
        name: String::new(),
        error: None,
    })?;
    if role.is_none() {
        *r.status_mut() = StatusCode::NOT_FOUND;
    }
    Ok(r)
}

#[derive(Deserialize)]
pub struct InviteForm {
    email: String,
    name: String,
    password: String,
    password2: String,
}

pub async fn accept_invite(State(st): State<AppState>, Path(token): Path<String>, Form(f): Form<InviteForm>) -> Page {
    let again = |error: String, valid: bool| InvitePage {
        token: token.clone(),
        valid,
        admin: false,
        email: f.email.clone(),
        name: f.name.clone(),
        error: Some(error),
    };
    if f.password != f.password2 {
        return html(again("The passwords do not match.".into(), true));
    }
    match users::accept_invite(&st.pool, &token, &f.email, &f.name, &f.password).await {
        Ok(id) => {
            let cookie = session::create(&st.pool, id, st.cookie_secure).await?;
            Ok(([(SET_COOKIE, cookie)], Redirect::to("/")).into_response())
        }
        Err(AcceptError::Input(m)) => html(again(m, true)),
        Err(AcceptError::Invalid) => html(again("This invite link is used up or has expired.".into(), false)),
    }
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsPage {
    nav: Nav,
    email: String,
    name: String,
    tz: String,
    zones: Vec<&'static str>,
    notice: Option<&'static str>,
}

#[derive(Deserialize)]
pub struct Notice {
    saved: Option<String>,
}

pub async fn settings(user: WebUser, Query(q): Query<Notice>) -> Page {
    let notice = match q.saved.as_deref() {
        Some("profile") => Some("Profile saved."),
        Some("password") => Some("Password changed. Other browsers were logged out."),
        _ => None,
    };
    html(SettingsPage {
        nav: Nav::new(&user, "settings"),
        email: user.email.clone(),
        name: user.name.clone(),
        tz: user.tz.name().to_string(),
        zones: chrono_tz::TZ_VARIANTS.iter().map(|z| z.name()).collect(),
        notice,
    })
}

#[derive(Deserialize)]
pub struct ProfileForm {
    name: String,
    tz: String,
}

pub async fn save_profile(State(st): State<AppState>, user: WebUser, Form(f): Form<ProfileForm>) -> Page {
    if f.tz.parse::<chrono_tz::Tz>().is_err() {
        return Err(ApiError::bad("unknown time zone").into());
    }
    sqlx::query("UPDATE users SET display_name = $2, tz = $3 WHERE id = $1")
        .bind(user.id)
        .bind(f.name.trim().chars().take(60).collect::<String>())
        .bind(&f.tz)
        .execute(&st.pool)
        .await?;
    // Tokens per day follow the person's time zone.
    st.reload().await?;
    Ok(Redirect::to("/settings?saved=profile").into_response())
}

#[derive(Deserialize)]
pub struct PasswordForm {
    current: String,
    password: String,
    password2: String,
}

/// Needs the current password: a borrowed, unlocked browser must not be
/// enough to take the account over. Ends every other session.
pub async fn change_password(
    State(st): State<AppState>,
    ClientAddr(addr): ClientAddr,
    user: WebUser,
    Form(f): Form<PasswordForm>,
) -> Page {
    if !st.logins.allowed(addr, &user.email, Instant::now()) {
        return Err(ApiError::Busy("too many failed attempts; wait five minutes".into()).into());
    }
    if session::check_login(&st.pool, &user.email, &f.current).await?.is_none() {
        st.logins.failed(addr, &user.email, Instant::now());
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        return Err(ApiError::bad("the current password is wrong").into());
    }
    if f.password != f.password2 {
        return Err(ApiError::bad("the new passwords do not match").into());
    }
    users::check_password(&f.password).map_err(|e| ApiError::bad(e.to_string()))?;
    sqlx::query("UPDATE users SET password_hash = $2 WHERE id = $1")
        .bind(user.id)
        .bind(session::hash_password(&f.password))
        .execute(&st.pool)
        .await?;
    sqlx::query("DELETE FROM web_sessions WHERE user_id = $1 AND token_sha256 <> $2")
        .bind(user.id)
        .bind(crate::auth::secret_hash(&user.token))
        .execute(&st.pool)
        .await?;
    Ok(Redirect::to("/settings?saved=password").into_response())
}
