//! The usage writer. Requests hand it a record and move on: the channel is
//! bounded and a full one drops the record (counted), so bookkeeping never
//! slows a request down. One task inserts in batches and updates each key's
//! `last_used_at` once per batch.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::{PgPool, QueryBuilder};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

const CAPACITY: usize = 10_000;
const BATCH: usize = 500;
const EVERY: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub struct Record {
    pub at: DateTime<Utc>,
    pub principal: Uuid,
    /// None: an admin's Try it on the website.
    pub key: Option<Uuid>,
    pub model: Uuid,
    /// The model that answered, also when a persona was asked for.
    pub model_name: String,
    pub persona: Option<Uuid>,
    pub persona_name: Option<String>,
    pub connection: Uuid,
    pub endpoint: &'static str,
    pub status: u16,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cached_tokens: i64,
    pub ttfb_ms: Option<i32>,
    pub total_ms: i32,
    pub usage_reported: bool,
    pub client_closed: bool,
    /// The limit that turned it away (`rpm`, `tokens`, `parallel`).
    pub limited: Option<&'static str>,
}

enum Msg {
    Record(Box<Record>),
    /// Write everything queued before this, then answer.
    Flush(oneshot::Sender<()>),
}

#[derive(Clone)]
pub struct UsageSink {
    tx: mpsc::Sender<Msg>,
    dropped: Arc<AtomicU64>,
}

impl UsageSink {
    /// Starts the writer task on `pool`.
    pub fn start(pool: PgPool) -> Self {
        let (tx, rx) = mpsc::channel(CAPACITY);
        tokio::spawn(run(pool, rx));
        UsageSink { tx, dropped: Default::default() }
    }

    pub fn record(&self, r: Record) {
        if self.tx.try_send(Msg::Record(Box::new(r))).is_err() {
            let n = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if n == 1 || n.is_multiple_of(1000) {
                tracing::warn!(dropped = n, "usage writer is behind; records dropped");
            }
        }
    }

    /// Records waiting for the writer.
    pub fn queued(&self) -> usize {
        self.tx.max_capacity() - self.tx.capacity()
    }

    /// Records dropped because the writer was behind, since start.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Waits until everything recorded so far is in Postgres. Shutdown and
    /// tests use it.
    pub async fn flush(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Msg::Flush(tx)).await.is_ok() {
            let _ = rx.await;
        }
    }
}

async fn run(pool: PgPool, mut rx: mpsc::Receiver<Msg>) {
    let mut batch: Vec<Record> = Vec::with_capacity(BATCH);
    loop {
        let msg = if batch.is_empty() {
            rx.recv().await
        } else {
            match tokio::time::timeout(EVERY, rx.recv()).await {
                Ok(m) => m,
                Err(_) => {
                    write(&pool, &mut batch).await;
                    continue;
                }
            }
        };
        match msg {
            None => {
                write(&pool, &mut batch).await;
                return;
            }
            Some(Msg::Record(r)) => {
                batch.push(*r);
                if batch.len() >= BATCH {
                    write(&pool, &mut batch).await;
                }
            }
            Some(Msg::Flush(done)) => {
                write(&pool, &mut batch).await;
                let _ = done.send(());
            }
        }
    }
}

async fn write(pool: &PgPool, batch: &mut Vec<Record>) {
    if batch.is_empty() {
        return;
    }
    let mut q = QueryBuilder::new(
        "INSERT INTO usage (at, principal_id, key_id, model_id, model_name, connection_id, endpoint, status,
         prompt_tokens, completion_tokens, cached_tokens, ttfb_ms, total_ms, usage_reported, client_closed,
         persona_id, persona_name, limited) ",
    );
    q.push_values(batch.iter(), |mut b, r| {
        b.push_bind(r.at)
            .push_bind(r.principal)
            .push_bind(r.key)
            .push_bind(r.model)
            .push_bind(&r.model_name)
            .push_bind(r.connection)
            .push_bind(r.endpoint)
            .push_bind(i32::from(r.status))
            .push_bind(r.prompt_tokens)
            .push_bind(r.completion_tokens)
            .push_bind(r.cached_tokens)
            .push_bind(r.ttfb_ms)
            .push_bind(r.total_ms)
            .push_bind(r.usage_reported)
            .push_bind(r.client_closed)
            .push_bind(r.persona)
            .push_bind(&r.persona_name)
            .push_bind(r.limited);
    });
    // A key, model, persona or connection deleted while its request ran fails the
    // foreign key for the whole batch; the rows are still worth keeping, so
    // they go in one by one with those ids unset.
    if let Err(e) = q.build().execute(pool).await {
        tracing::error!(error = %e, rows = batch.len(), "writing usage failed; trying rows one by one");
        for r in batch.iter() {
            write_one(pool, r).await;
        }
    }
    let mut keys: Vec<Uuid> = batch.iter().filter_map(|r| r.key).collect();
    keys.sort();
    keys.dedup();
    if let Err(e) =
        sqlx::query("UPDATE api_keys SET last_used_at = now() WHERE id = ANY($1)").bind(&keys).execute(pool).await
    {
        tracing::error!(error = %e, "updating last_used_at failed");
    }
    batch.clear();
}

/// The fallback for a failed batch: ids that no longer exist become NULL.
async fn write_one(pool: &PgPool, r: &Record) {
    let res = sqlx::query(
        "INSERT INTO usage (at, principal_id, key_id, model_id, model_name, connection_id, endpoint, status,
         prompt_tokens, completion_tokens, cached_tokens, ttfb_ms, total_ms, usage_reported, client_closed,
         persona_id, persona_name, limited)
         VALUES ($1, (SELECT id FROM principals WHERE id = $2), (SELECT id FROM api_keys WHERE id = $3),
                 (SELECT id FROM models WHERE id = $4), $5, (SELECT id FROM connections WHERE id = $6),
                 $7, $8, $9, $10, $11, $12, $13, $14, $15, (SELECT id FROM personas WHERE id = $16), $17, $18)",
    )
    .bind(r.at)
    .bind(r.principal)
    .bind(r.key)
    .bind(r.model)
    .bind(&r.model_name)
    .bind(r.connection)
    .bind(r.endpoint)
    .bind(i32::from(r.status))
    .bind(r.prompt_tokens)
    .bind(r.completion_tokens)
    .bind(r.cached_tokens)
    .bind(r.ttfb_ms)
    .bind(r.total_ms)
    .bind(r.usage_reported)
    .bind(r.client_closed)
    .bind(r.persona)
    .bind(&r.persona_name)
    .bind(r.limited)
    .execute(pool)
    .await;
    if let Err(e) = res {
        tracing::error!(error = %e, "dropping one usage row");
    }
}
