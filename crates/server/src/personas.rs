//! Layer 2: personas. Storage, validation and history. The request path
//! reads personas from the snapshot; the rules that change a request live
//! in `core::persona`.

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use converge_core::persona::{Params, PromptMode};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::connections::claim_public_id;

/// Long enough for a serious prompt, short enough to stay a prompt.
pub const PROMPT_MAX: usize = 100_000;

/// What the persona form submits, already checked.
pub struct PersonaInput {
    pub public_id: String,
    pub title: String,
    pub description: String,
    pub model_id: Uuid,
    pub system_prompt: String,
    pub prompt_mode: PromptMode,
    pub prompt_visible: bool,
    pub params: Params,
    pub enabled: bool,
}

pub fn check_title(t: &str) -> Result<String> {
    let t = t.trim();
    if t.is_empty() || t.chars().count() > 80 {
        bail!("give the persona a title of at most 80 characters");
    }
    Ok(t.to_string())
}

pub fn check_description(d: &str) -> Result<String> {
    let d = d.trim();
    if d.chars().count() > 500 {
        bail!("keep the description under 500 characters");
    }
    Ok(d.to_string())
}

/// Line endings normalised (browsers send CRLF), otherwise kept as written.
pub fn check_prompt(p: &str) -> Result<String> {
    let p = p.replace("\r\n", "\n");
    if p.chars().count() > PROMPT_MAX {
        bail!("the prompt is longer than {PROMPT_MAX} characters");
    }
    Ok(p.trim_end().to_string())
}

/// Who made a change, for the history.
#[derive(Clone, Copy)]
pub struct By<'a> {
    pub id: Uuid,
    pub name: &'a str,
}

async fn model_name(tx: &mut sqlx::PgConnection, id: Uuid) -> Result<String> {
    let name: Option<String> =
        sqlx::query_scalar("SELECT public_id FROM models WHERE id = $1").bind(id).fetch_optional(tx).await?;
    name.ok_or_else(|| anyhow::anyhow!("pick a model from the list"))
}

pub async fn create(pool: &PgPool, by: By<'_>, input: &PersonaInput) -> Result<Uuid> {
    let mut tx = pool.begin().await?;
    claim_public_id(&mut tx, &input.public_id, None).await?;
    let model = model_name(&mut tx, input.model_id).await?;
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO personas (id, public_id, title, description, model_id, system_prompt, prompt_mode, prompt_visible, params, enabled)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(id)
    .bind(&input.public_id)
    .bind(&input.title)
    .bind(&input.description)
    .bind(input.model_id)
    .bind(&input.system_prompt)
    .bind(input.prompt_mode.as_str())
    .bind(input.prompt_visible)
    .bind(input.params.to_json())
    .bind(input.enabled)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO persona_history (persona_id, by_user, by_name, what, to_model_name) VALUES ($1, $2, $3, 'created', $4)")
        .bind(id)
        .bind(by.id)
        .bind(by.name)
        .bind(model)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(id)
}

/// Saves the whole form. Each kind of change that happened (model, prompt,
/// settings) gets a history row with what it replaced.
pub async fn update(pool: &PgPool, by: By<'_>, id: Uuid, input: &PersonaInput) -> Result<()> {
    let mut tx = pool.begin().await?;
    let old: Option<(String, Uuid, String, String, Value)> = sqlx::query_as(
        "SELECT public_id, model_id, system_prompt, prompt_mode, params FROM personas WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((old_id, old_model, old_prompt, old_mode, old_params)) = old else { bail!("no such persona") };
    if input.public_id != old_id {
        claim_public_id(&mut tx, &input.public_id, Some(id)).await?;
    }
    let new_model = model_name(&mut tx, input.model_id).await?;
    sqlx::query(
        "UPDATE personas SET public_id = $2, title = $3, description = $4, model_id = $5, system_prompt = $6,
                prompt_mode = $7, prompt_visible = $8, params = $9, enabled = $10, updated_at = now()
          WHERE id = $1",
    )
    .bind(id)
    .bind(&input.public_id)
    .bind(&input.title)
    .bind(&input.description)
    .bind(input.model_id)
    .bind(&input.system_prompt)
    .bind(input.prompt_mode.as_str())
    .bind(input.prompt_visible)
    .bind(input.params.to_json())
    .bind(input.enabled)
    .execute(&mut *tx)
    .await?;

    if input.model_id != old_model {
        let from = model_name(&mut tx, old_model).await?;
        sqlx::query(
            "INSERT INTO persona_history (persona_id, by_user, by_name, what, from_model, from_model_name, to_model_name)
             VALUES ($1, $2, $3, 'model', $4, $5, $6)",
        )
        .bind(id)
        .bind(by.id)
        .bind(by.name)
        .bind(old_model)
        .bind(from)
        .bind(&new_model)
        .execute(&mut *tx)
        .await?;
    }
    if input.system_prompt != old_prompt || input.prompt_mode.as_str() != old_mode {
        sqlx::query(
            "INSERT INTO persona_history (persona_id, by_user, by_name, what, prev_prompt, prev_mode)
             VALUES ($1, $2, $3, 'prompt', $4, $5)",
        )
        .bind(id)
        .bind(by.id)
        .bind(by.name)
        .bind(old_prompt)
        .bind(old_mode)
        .execute(&mut *tx)
        .await?;
    }
    if input.params != Params::from_json(&old_params) {
        sqlx::query(
            "INSERT INTO persona_history (persona_id, by_user, by_name, what, prev_params) VALUES ($1, $2, $3, 'settings', $4)",
        )
        .bind(id)
        .bind(by.id)
        .bind(by.name)
        .bind(old_params)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// A persona, its grants, key scopes and history.
pub async fn delete(pool: &PgPool, id: Uuid) -> Result<()> {
    let mut tx = pool.begin().await?;
    for table in ["grants", "key_scopes"] {
        sqlx::query(&format!("DELETE FROM {table} WHERE target_kind = 'persona' AND target_id = $1"))
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("DELETE FROM personas WHERE id = $1").bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// A persona as the website shows it.
#[derive(sqlx::FromRow)]
pub struct PersonaRow {
    pub id: Uuid,
    pub public_id: String,
    pub title: String,
    pub description: String,
    pub model_id: Uuid,
    pub model_public_id: String,
    pub connection_name: String,
    pub system_prompt: String,
    pub prompt_mode: String,
    pub prompt_visible: bool,
    pub params: Value,
    pub enabled: bool,
    /// Its model and connection are on: what the snapshot also decides.
    /// Off means paused.
    pub model_live: bool,
}

const SELECT: &str = "SELECT p.id, p.public_id, p.title, p.description, p.model_id, m.public_id AS model_public_id,
        c.name AS connection_name, p.system_prompt, p.prompt_mode, p.prompt_visible, p.params, p.enabled,
        (m.enabled AND c.enabled) AS model_live
   FROM personas p JOIN models m ON m.id = p.model_id JOIN connections c ON c.id = m.connection_id";

pub async fn get(pool: &PgPool, id: Uuid) -> Result<Option<PersonaRow>> {
    Ok(sqlx::query_as(&format!("{SELECT} WHERE p.id = $1")).bind(id).fetch_optional(pool).await?)
}

pub async fn list(pool: &PgPool) -> Result<Vec<PersonaRow>> {
    Ok(sqlx::query_as(&format!("{SELECT} ORDER BY p.title, p.public_id")).fetch_all(pool).await?)
}

impl PersonaRow {
    /// The form's values as they are now, to change one part and save.
    pub fn input(&self) -> PersonaInput {
        PersonaInput {
            public_id: self.public_id.clone(),
            title: self.title.clone(),
            description: self.description.clone(),
            model_id: self.model_id,
            system_prompt: self.system_prompt.clone(),
            prompt_mode: PromptMode::parse(&self.prompt_mode),
            prompt_visible: self.prompt_visible,
            params: Params::from_json(&self.params),
            enabled: self.enabled,
        }
    }
}

#[derive(sqlx::FromRow)]
pub struct HistoryRow {
    pub id: i64,
    pub at: DateTime<Utc>,
    pub by_name: String,
    pub what: String,
    pub from_model: Option<Uuid>,
    pub from_model_name: Option<String>,
    pub to_model_name: Option<String>,
    pub prev_prompt: Option<String>,
    pub prev_mode: Option<String>,
    pub prev_params: Option<Value>,
}

/// The latest changes, newest first.
pub async fn history(pool: &PgPool, persona: Uuid) -> Result<Vec<HistoryRow>> {
    Ok(sqlx::query_as(
        "SELECT id, at, by_name, what, from_model, from_model_name, to_model_name, prev_prompt, prev_mode, prev_params
           FROM persona_history WHERE persona_id = $1 ORDER BY at DESC, id DESC LIMIT 50",
    )
    .bind(persona)
    .fetch_all(pool)
    .await?)
}

/// Undoes one change: back to the model, prompt or settings it replaced.
/// The undo is itself a change, so it lands in the history too.
pub async fn restore(pool: &PgPool, by: By<'_>, persona: Uuid, entry: i64) -> Result<()> {
    let h: Option<HistoryRow> = sqlx::query_as(
        "SELECT id, at, by_name, what, from_model, from_model_name, to_model_name, prev_prompt, prev_mode, prev_params
           FROM persona_history WHERE id = $1 AND persona_id = $2",
    )
    .bind(entry)
    .bind(persona)
    .fetch_optional(pool)
    .await?;
    let Some(h) = h else { bail!("no such change") };
    let Some(row) = get(pool, persona).await? else { bail!("no such persona") };
    let mut input = row.input();
    match h.what.as_str() {
        "model" => {
            let Some(m) = h.from_model else { bail!("that model has been removed since") };
            input.model_id = m;
        }
        "prompt" => {
            input.system_prompt = h.prev_prompt.unwrap_or_default();
            input.prompt_mode = PromptMode::parse(h.prev_mode.as_deref().unwrap_or("prepend"));
        }
        "settings" => input.params = Params::from_json(&h.prev_params.unwrap_or(Value::Null)),
        _ => bail!("there is nothing to restore for this change"),
    }
    update(pool, by, persona, &input).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks() {
        assert_eq!(check_title("  Designer ").unwrap(), "Designer");
        assert!(check_title(" ").is_err());
        assert!(check_title(&"x".repeat(81)).is_err());
        assert!(check_description(&"x".repeat(501)).is_err());
        assert_eq!(check_prompt("Line one\r\nLine two\r\n\r\n").unwrap(), "Line one\nLine two");
        assert!(check_prompt(&"x".repeat(PROMPT_MAX + 1)).is_err());
    }
}
