//! Admin: people and invites.

use askama::Template;
use axum::Form;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use uuid::Uuid;

use super::session::AdminUser;
use super::{Nav, html, local_day};
use crate::AppState;
use crate::auth::Role;
use crate::error::{ApiError, WebError};
use crate::users;

type Page = Result<Response, WebError>;

#[derive(sqlx::FromRow)]
struct UserDb {
    id: Uuid,
    email: String,
    display_name: String,
    role: String,
    created_at: DateTime<Utc>,
    disabled_at: Option<DateTime<Utc>>,
}

struct UserRow {
    id: Uuid,
    email: String,
    name: String,
    admin: bool,
    created: String,
    disabled: bool,
    me: bool,
}

struct InviteRow {
    id: Uuid,
    role: String,
    note: String,
    expires: String,
}

#[derive(Template)]
#[template(path = "admin_people.html")]
struct PeoplePage {
    nav: Nav,
    users: Vec<UserRow>,
    invites: Vec<InviteRow>,
}

pub async fn people(State(st): State<AppState>, AdminUser(me): AdminUser) -> Page {
    let rows: Vec<UserDb> =
        sqlx::query_as("SELECT id, email, display_name, role, created_at, disabled_at FROM users ORDER BY created_at")
            .fetch_all(&st.pool)
            .await?;
    let users = rows
        .into_iter()
        .map(|r| UserRow {
            id: r.id,
            me: r.id == me.id,
            email: r.email,
            name: r.display_name,
            admin: r.role == "admin",
            created: local_day(r.created_at, me.tz),
            disabled: r.disabled_at.is_some(),
        })
        .collect();
    let inv: Vec<(Uuid, String, String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT id, role, note, expires_at FROM invites WHERE used_at IS NULL AND expires_at > now() ORDER BY created_at DESC",
    )
    .fetch_all(&st.pool)
    .await?;
    let invites = inv
        .into_iter()
        .map(|(id, role, note, expires)| InviteRow { id, role, note, expires: local_day(expires, me.tz) })
        .collect();
    html(PeoplePage { nav: Nav::new(&me, "admin"), users, invites })
}

#[derive(Template)]
#[template(path = "invite_created.html")]
struct InviteCreated {
    link: String,
    expires: String,
    admin: bool,
}

#[derive(Deserialize)]
pub struct InviteForm {
    role: String,
    note: String,
}

pub async fn create_invite(
    State(st): State<AppState>,
    AdminUser(me): AdminUser,
    headers: HeaderMap,
    Form(f): Form<InviteForm>,
) -> Page {
    let role = Role::parse(&f.role);
    let (token, expires) = users::create_invite(&st.pool, me.id, role, &f.note).await.map_err(ApiError::Internal)?;
    html(InviteCreated {
        link: format!("{}/invite/{token}", st.public_base(&headers)),
        expires: local_day(expires, me.tz),
        admin: role == Role::Admin,
    })
}

pub async fn delete_invite(State(st): State<AppState>, _a: AdminUser, Path(id): Path<Uuid>) -> Page {
    sqlx::query("DELETE FROM invites WHERE id = $1 AND used_at IS NULL").bind(id).execute(&st.pool).await?;
    Ok("".into_response())
}

#[derive(Deserialize)]
pub struct RoleForm {
    role: String,
}

pub async fn set_role(
    State(st): State<AppState>,
    AdminUser(me): AdminUser,
    Path(id): Path<Uuid>,
    Form(f): Form<RoleForm>,
) -> Page {
    let role = Role::parse(&f.role);
    if role == Role::User && id == me.id && users::other_admins(&st.pool, me.id).await? == 0 {
        return Err(ApiError::bad("you are the only admin; make someone else admin first").into());
    }
    sqlx::query("UPDATE users SET role = $2 WHERE id = $1").bind(id).bind(role.as_str()).execute(&st.pool).await?;
    st.reload().await?;
    Ok(Redirect::to("/admin/people").into_response())
}

/// Disabling ends the person's web sessions and stops their keys at once
/// (the snapshot leaves out disabled people's keys). Enabling restores both.
pub async fn disable(State(st): State<AppState>, AdminUser(me): AdminUser, Path(id): Path<Uuid>) -> Page {
    if id == me.id {
        return Err(ApiError::bad("you cannot disable yourself").into());
    }
    sqlx::query("UPDATE users SET disabled_at = now() WHERE id = $1 AND disabled_at IS NULL")
        .bind(id)
        .execute(&st.pool)
        .await?;
    sqlx::query("DELETE FROM web_sessions WHERE user_id = $1").bind(id).execute(&st.pool).await?;
    st.reload().await?;
    Ok(Redirect::to("/admin/people").into_response())
}

pub async fn enable(State(st): State<AppState>, _a: AdminUser, Path(id): Path<Uuid>) -> Page {
    sqlx::query("UPDATE users SET disabled_at = NULL WHERE id = $1").bind(id).execute(&st.pool).await?;
    st.reload().await?;
    Ok(Redirect::to("/admin/people").into_response())
}
