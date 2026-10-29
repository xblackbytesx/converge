//! The Usage page and its CSV: totals, per day, breakdowns and the recent
//! requests for a period, in the viewer's time zone. Users see their own,
//! admins everyone's (or one person's). Counts only: nothing here can show
//! a prompt or a reply, because none is stored (decision 6).

use askama::Template;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

use super::keys::when;
use super::session::WebUser;
use super::{Nav, html};
use crate::AppState;
use crate::error::{ApiError, WebError};

type Page = Result<Response, WebError>;

/// Recent requests per page.
const RECENT: i64 = 100;
/// CSV rows per database round trip.
const CSV_PAGE: i64 = 2000;

#[derive(Deserialize, Default)]
pub struct Filter {
    /// `today`, `7d` (default), `30d`, or a month as `YYYY-MM`.
    range: Option<String>,
    /// A person's id (admins only); empty: everyone.
    who: Option<String>,
    /// Recent requests older than this usage id.
    before: Option<i64>,
}

/// What the filter resolved to.
struct Period {
    key: String,
    label: String,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    days: Vec<NaiveDate>,
}

fn midnight(d: NaiveDate, tz: Tz) -> DateTime<Utc> {
    let t = d.and_hms_opt(0, 0, 0).expect("midnight exists");
    tz.from_local_datetime(&t).earliest().map_or_else(|| Utc.from_utc_datetime(&t), |t| t.with_timezone(&Utc))
}

fn month_start(y: i32, m: u32) -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(y, m, 1)
}

fn next_month(d: NaiveDate) -> NaiveDate {
    if d.month() == 12 { month_start(d.year() + 1, 1) } else { month_start(d.year(), d.month() + 1) }
        .expect("a valid month")
}

fn period(range: Option<&str>, now: DateTime<Utc>, tz: Tz) -> Period {
    let today = now.with_timezone(&tz).date_naive();
    let span = |key: &str, label: &str, first: NaiveDate, end: NaiveDate| Period {
        key: key.into(),
        label: label.into(),
        from: midnight(first, tz),
        to: midnight(end, tz),
        days: first.iter_days().take_while(|d| *d < end).collect(),
    };
    let last =
        |n: i64, key: &str, label: &str| span(key, label, today - Duration::days(n - 1), today + Duration::days(1));
    match range.unwrap_or("7d") {
        "today" => last(1, "today", "Today"),
        "30d" => last(30, "30d", "Last 30 days"),
        r => {
            let month = r.split_once('-').and_then(|(y, m)| month_start(y.parse().ok()?, m.parse().ok()?));
            match month {
                Some(first) => span(r, &first.format("%B %Y").to_string(), first, next_month(first)),
                None => last(7, "7d", "Last 7 days"),
            }
        }
    }
}

/// The range choices: three spans and the last twelve months.
fn ranges(now: DateTime<Utc>, tz: Tz, current: &str) -> Vec<Choice> {
    let mut v: Vec<(String, String)> =
        [("today", "Today"), ("7d", "Last 7 days"), ("30d", "Last 30 days")].map(|(k, l)| (k.into(), l.into())).into();
    let today = now.with_timezone(&tz).date_naive();
    let mut m = month_start(today.year(), today.month()).expect("a valid month");
    for _ in 0..12 {
        v.push((m.format("%Y-%m").to_string(), m.format("%B %Y").to_string()));
        m = month_start(
            if m.month() == 1 { m.year() - 1 } else { m.year() },
            if m.month() == 1 { 12 } else { m.month() - 1 },
        )
        .expect("a valid month");
    }
    v.into_iter().map(|(value, label)| Choice { selected: value == current, value, label }).collect()
}

/// Thousands grouped with no-break spaces: `12 400`.
pub fn group(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('\u{a0}');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

struct Choice {
    value: String,
    label: String,
    selected: bool,
}

struct Tile {
    label: &'static str,
    value: String,
    sub: String,
}

struct DayRow {
    day: String,
    requests: String,
    tokens: String,
    value: i64,
}

struct GroupRow {
    label: String,
    sub: String,
    requests: String,
    tokens_in: String,
    cached: String,
    tokens_out: String,
}

struct Breakdown {
    title: &'static str,
    head: &'static str,
    rows: Vec<GroupRow>,
}

struct RecentRow {
    at: String,
    who: String,
    model: String,
    persona: Option<String>,
    endpoint: String,
    status: i32,
    tokens: String,
    ttfb: String,
    total: String,
    note: String,
}

#[derive(Template)]
#[template(path = "usage.html")]
struct UsagePage {
    nav: Nav,
    admin: bool,
    period: String,
    /// Admins: `everyone` or the person's email.
    whose: String,
    ranges: Vec<Choice>,
    people: Vec<Choice>,
    /// The filter as a query string, for the CSV link and paging.
    query: String,
    tiles: Vec<Tile>,
    limits: Vec<String>,
    days: Vec<DayRow>,
    max_day: i64,
    breakdowns: Vec<Breakdown>,
    rows: Vec<RecentRow>,
    older: Option<i64>,
}

/// Whose usage: users always their own; admins everyone's or one person's.
fn principal(user: &WebUser, f: &Filter) -> Result<Option<Uuid>, ApiError> {
    if !user.is_admin() {
        return Ok(Some(user.id));
    }
    match f.who.as_deref().map(str::trim).filter(|w| !w.is_empty()) {
        None => Ok(None),
        Some(w) => w.parse().map(Some).map_err(|_| ApiError::bad("unknown person")),
    }
}

fn query_string(p: &Period, who: Option<Uuid>, admin: bool) -> String {
    let mut q = format!("range={}", p.key);
    if let (true, Some(w)) = (admin, who) {
        q.push_str(&format!("&who={w}"));
    }
    q
}

/// The filter every query shares: $1 from, $2 to, $3 principal or NULL.
const WHERE: &str = "u.at >= $1 AND u.at < $2 AND ($3::uuid IS NULL OR u.principal_id = $3)";

#[derive(sqlx::FromRow)]
struct Totals {
    requests: i64,
    errors: i64,
    prompt: i64,
    cached: i64,
    completion: i64,
    ttfb_p50: Option<f64>,
}

#[derive(sqlx::FromRow)]
struct Grouped {
    label: Option<String>,
    sub: Option<String>,
    requests: i64,
    prompt: i64,
    cached: i64,
    completion: i64,
}

/// One breakdown: `label` and `sub` are SQL over `u` and `join`.
async fn grouped(
    pool: &PgPool,
    p: &Period,
    who: Option<Uuid>,
    label: &str,
    sub: &str,
    join: &str,
) -> Result<Vec<Grouped>, sqlx::Error> {
    // Only constant fragments are formatted in; values are bound.
    sqlx::query_as(&format!(
        "SELECT {label} AS label, {sub} AS sub, count(*)::bigint AS requests,
                sum(u.prompt_tokens)::bigint AS prompt, sum(u.cached_tokens)::bigint AS cached,
                sum(u.completion_tokens)::bigint AS completion
           FROM usage u {join}
          WHERE {WHERE}
          GROUP BY 1, 2
          ORDER BY sum(u.prompt_tokens + u.completion_tokens) DESC, count(*) DESC, 1"
    ))
    .bind(p.from)
    .bind(p.to)
    .bind(who)
    .fetch_all(pool)
    .await
}

fn rows(g: Vec<Grouped>, none: &str) -> Vec<GroupRow> {
    g.into_iter()
        .map(|g| GroupRow {
            label: g.label.unwrap_or_else(|| none.to_string()),
            sub: g.sub.unwrap_or_default(),
            requests: group(g.requests),
            tokens_in: group(g.prompt),
            cached: if g.cached > 0 { group(g.cached) } else { String::new() },
            tokens_out: group(g.completion),
        })
        .collect()
}

#[derive(sqlx::FromRow)]
struct UsageDb {
    id: i64,
    at: DateTime<Utc>,
    who: Option<String>,
    key_name: Option<String>,
    model_name: String,
    persona_name: Option<String>,
    endpoint: String,
    status: i32,
    prompt_tokens: i64,
    completion_tokens: i64,
    cached_tokens: i64,
    ttfb_ms: Option<i32>,
    total_ms: i32,
    usage_reported: bool,
    client_closed: bool,
    limited: Option<String>,
}

const ROWS: &str = "SELECT u.id, u.at, p.email AS who, k.name AS key_name, u.model_name, u.persona_name, u.endpoint,
        u.status, u.prompt_tokens, u.completion_tokens, u.cached_tokens, u.ttfb_ms, u.total_ms,
        u.usage_reported, u.client_closed, u.limited
   FROM usage u LEFT JOIN users p ON p.id = u.principal_id LEFT JOIN api_keys k ON k.id = u.key_id";

pub async fn page(State(st): State<AppState>, user: WebUser, Query(f): Query<Filter>) -> Page {
    let who = principal(&user, &f)?;
    let now = Utc::now();
    let p = period(f.range.as_deref(), now, user.tz);
    let tz = user.tz.name();

    let t: Totals = sqlx::query_as(&format!(
        "SELECT count(*)::bigint AS requests, (count(*) FILTER (WHERE u.status >= 400))::bigint AS errors,
                coalesce(sum(u.prompt_tokens), 0)::bigint AS prompt,
                coalesce(sum(u.cached_tokens), 0)::bigint AS cached,
                coalesce(sum(u.completion_tokens), 0)::bigint AS completion,
                (percentile_cont(0.5) WITHIN GROUP (ORDER BY u.ttfb_ms))::float8 AS ttfb_p50
           FROM usage u WHERE {WHERE}"
    ))
    .bind(p.from)
    .bind(p.to)
    .bind(who)
    .fetch_one(&st.pool)
    .await?;
    let share = if t.prompt > 0 { format!("{}% cached", t.cached * 100 / t.prompt) } else { String::new() };
    let tiles = vec![
        Tile { label: "Requests", value: group(t.requests), sub: String::new() },
        Tile {
            label: "Errors",
            value: group(t.errors),
            sub: if t.errors > 0 { "including limits".into() } else { String::new() },
        },
        Tile { label: "Tokens in", value: group(t.prompt), sub: share },
        Tile { label: "Tokens out", value: group(t.completion), sub: String::new() },
        Tile {
            label: "First byte",
            value: t.ttfb_p50.map(|m| format!("{} ms", m.round() as i64)).unwrap_or_else(|| "-".into()),
            sub: "median".into(),
        },
    ];

    let per_day: Vec<(NaiveDate, i64, i64)> = sqlx::query_as(&format!(
        "SELECT (u.at AT TIME ZONE $4)::date, count(*)::bigint, sum(u.prompt_tokens + u.completion_tokens)::bigint
           FROM usage u WHERE {WHERE} GROUP BY 1"
    ))
    .bind(p.from)
    .bind(p.to)
    .bind(who)
    .bind(tz)
    .fetch_all(&st.pool)
    .await?;
    let days: Vec<DayRow> = p
        .days
        .iter()
        .rev()
        .map(|d| {
            let (r, tok) = per_day.iter().find(|x| x.0 == *d).map_or((0, 0), |x| (x.1, x.2));
            DayRow { day: d.format("%a %Y-%m-%d").to_string(), requests: group(r), tokens: group(tok), value: tok }
        })
        .collect();
    let max_day = days.iter().map(|d| d.value).max().unwrap_or(0).max(1);

    let mut breakdowns = vec![
        Breakdown {
            title: "By model or persona",
            head: "Model or persona",
            rows: rows(
                grouped(
                    &st.pool,
                    &p,
                    who,
                    "coalesce(u.persona_name, u.model_name)",
                    "CASE WHEN u.persona_name IS NOT NULL THEN 'on ' || u.model_name END",
                    "",
                )
                .await?,
                "",
            ),
        },
        Breakdown {
            title: "By key",
            head: "Key",
            rows: rows(
                grouped(
                    &st.pool,
                    &p,
                    who,
                    "CASE WHEN u.key_id IS NULL THEN 'Website (Playground)' ELSE coalesce(k.name, '(deleted)') END",
                    if user.is_admin() { "p.email" } else { "k.prefix || '…'" },
                    "LEFT JOIN api_keys k ON k.id = u.key_id LEFT JOIN users p ON p.id = u.principal_id",
                )
                .await?,
                "",
            ),
        },
    ];
    if user.is_admin() {
        breakdowns.push(Breakdown {
            title: "By person",
            head: "Person",
            rows: rows(
                grouped(&st.pool, &p, who, "p.email", "NULL::text", "LEFT JOIN users p ON p.id = u.principal_id")
                    .await?,
                "(deleted)",
            ),
        });
        breakdowns.push(Breakdown {
            title: "By connection",
            head: "Connection",
            rows: rows(
                grouped(&st.pool, &p, who, "c.name", "NULL::text", "LEFT JOIN connections c ON c.id = u.connection_id")
                    .await?,
                "(deleted)",
            ),
        });
    }

    let mut recent: Vec<UsageDb> = sqlx::query_as(&format!(
        "{ROWS} WHERE {WHERE} AND ($4::bigint IS NULL OR u.id < $4) ORDER BY u.id DESC LIMIT {}",
        RECENT + 1
    ))
    .bind(p.from)
    .bind(p.to)
    .bind(who)
    .bind(f.before)
    .fetch_all(&st.pool)
    .await?;
    let older = (recent.len() as i64 > RECENT).then(|| {
        recent.truncate(RECENT as usize);
        recent.last().map_or(0, |r| r.id)
    });
    let rows = recent.into_iter().map(|r| recent_row(r, user.tz)).collect();

    let limits = match who {
        Some(id) => limits_today(&st, id, now),
        None => vec![],
    };
    let people = if user.is_admin() {
        let all: Vec<(Uuid, String)> =
            sqlx::query_as("SELECT id, email FROM users ORDER BY email").fetch_all(&st.pool).await?;
        all.into_iter()
            .map(|(id, email)| Choice { selected: who == Some(id), value: id.to_string(), label: email })
            .collect()
    } else {
        vec![]
    };
    let whose = match (user.is_admin(), people.iter().find(|p| p.selected)) {
        (false, _) => String::new(),
        (true, Some(p)) => p.label.clone(),
        (true, None) => "everyone".into(),
    };
    html(UsagePage {
        whose,
        nav: Nav::new(&user, "usage"),
        admin: user.is_admin(),
        query: query_string(&p, who, user.is_admin()),
        ranges: ranges(now, user.tz, &p.key),
        period: p.label,
        people,
        tiles,
        limits,
        days,
        max_day,
        breakdowns,
        rows,
        older,
    })
}

/// Today against the person's limits, from the limiter's own counts.
fn limits_today(st: &AppState, id: Uuid, now: DateTime<Utc>) -> Vec<String> {
    let snap = st.gateway.load();
    let Some(l) = snap.limits.get(&id) else { return vec![] };
    let mut out = vec![];
    if let Some(n) = l.tokens_per_day {
        let used = st.limiter.used_today(id, l.tz, now);
        out.push(format!("Tokens today: {} of {} (resets at 00:00 {})", group(used), group(n), l.tz));
    }
    if let Some(n) = l.rpm {
        out.push(format!("Requests per minute: at most {}", group(n.into())));
    }
    if let Some(n) = l.parallel_per_key {
        out.push(format!("Parallel requests per key: at most {}", group(n.into())));
    }
    out
}

fn recent_row(r: UsageDb, tz: Tz) -> RecentRow {
    RecentRow {
        at: when(Some(r.at), tz, ""),
        who: r.who.unwrap_or_else(|| "(deleted)".into()),
        model: r.model_name,
        persona: r.persona_name,
        endpoint: r.endpoint,
        status: r.status,
        tokens: if r.usage_reported {
            let cached =
                if r.cached_tokens > 0 { format!(" ({} cached)", group(r.cached_tokens)) } else { String::new() };
            format!("{} in{cached} / {} out", group(r.prompt_tokens), group(r.completion_tokens))
        } else if r.limited.is_some() {
            String::new()
        } else {
            "not reported".into()
        },
        ttfb: r.ttfb_ms.map(|m| format!("{m} ms")).unwrap_or_default(),
        total: format!("{} ms", r.total_ms),
        note: match (&r.limited, r.client_closed) {
            (Some(l), _) => format!("limit: {}", limit_name(l)),
            (None, true) => "client went away".into(),
            (None, false) => String::new(),
        },
    }
}

fn limit_name(l: &str) -> &'static str {
    match l {
        "rpm" => "requests per minute",
        "tokens" => "tokens per day",
        "parallel" => "parallel requests",
        _ => "other",
    }
}

/// One CSV field: quoted when needed, and a leading `= + - @` defused so a
/// key name cannot become a spreadsheet formula.
fn csv(s: &str) -> String {
    let s = if s.starts_with(['=', '+', '-', '@']) { format!("'{s}") } else { s.to_string() };
    if s.contains([',', '"', '\n', '\r']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s }
}

const CSV_HEAD: &str = "time,person,key,model,persona,endpoint,status,prompt_tokens,cached_tokens,completion_tokens,ttfb_ms,total_ms,usage_reported,client_closed,limited\n";

fn csv_line(r: &UsageDb, tz: Tz) -> String {
    let opt = |v: &Option<String>| v.as_deref().map(csv).unwrap_or_default();
    format!(
        "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
        r.at.with_timezone(&tz).format("%Y-%m-%d %H:%M:%S"),
        opt(&r.who),
        opt(&r.key_name),
        csv(&r.model_name),
        opt(&r.persona_name),
        csv(&r.endpoint),
        r.status,
        r.prompt_tokens,
        r.cached_tokens,
        r.completion_tokens,
        r.ttfb_ms.map(|m| m.to_string()).unwrap_or_default(),
        r.total_ms,
        r.usage_reported,
        r.client_closed,
        opt(&r.limited),
    )
}

/// Every row of the period, oldest first, streamed a page at a time.
pub async fn csv_download(State(st): State<AppState>, user: WebUser, Query(f): Query<Filter>) -> Page {
    let who = principal(&user, &f)?;
    let p = period(f.range.as_deref(), Utc::now(), user.tz);
    let name = format!(
        "converge-usage-{}-to-{}.csv",
        p.days.first().map(|d| d.to_string()).unwrap_or_default(),
        p.days.last().map(|d| d.to_string()).unwrap_or_default()
    );
    let (pool, tz, from, to) = (st.pool.clone(), user.tz, p.from, p.to);
    let body = futures_util::stream::unfold(Some(0i64), move |after| {
        let pool = pool.clone();
        async move {
            let after = after?;
            let page: Result<Vec<UsageDb>, sqlx::Error> =
                sqlx::query_as(&format!("{ROWS} WHERE {WHERE} AND u.id > $4 ORDER BY u.id LIMIT {CSV_PAGE}"))
                    .bind(from)
                    .bind(to)
                    .bind(who)
                    .bind(after)
                    .fetch_all(&pool)
                    .await;
            match page {
                Err(e) => {
                    tracing::error!(error = %e, "usage CSV");
                    Some((Err(std::io::Error::other("database error")), None))
                }
                Ok(rows) if rows.is_empty() && after > 0 => None,
                Ok(rows) => {
                    let next = (rows.len() as i64 == CSV_PAGE).then(|| rows.last().map_or(after, |r| r.id));
                    let mut chunk = if after == 0 { CSV_HEAD.to_string() } else { String::new() };
                    for r in &rows {
                        chunk.push_str(&csv_line(r, tz));
                    }
                    Some((Ok(chunk), next))
                }
            }
        }
    });
    Ok((
        [
            (CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\"")),
        ],
        Body::from_stream(body),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periods_are_local_days() {
        let tz: Tz = "Europe/Amsterdam".parse().unwrap();
        // 00:30 on 29 October in Amsterdam (CET, UTC+1).
        let now: DateTime<Utc> = "2026-10-28T23:30:00Z".parse().unwrap();
        let p = period(Some("today"), now, tz);
        assert_eq!(
            (p.from.to_rfc3339(), p.to.to_rfc3339()),
            ("2026-10-28T23:00:00+00:00".into(), "2026-10-29T23:00:00+00:00".into())
        );
        let p = period(None, now, tz);
        assert_eq!((p.key.as_str(), p.days.len()), ("7d", 7));
        assert_eq!(p.days[0].to_string(), "2026-10-23");
        // October has the switch from CEST to CET.
        let p = period(Some("2026-10"), now, tz);
        assert_eq!(p.days.len(), 31);
        assert_eq!(p.from.to_rfc3339(), "2026-09-30T22:00:00+00:00");
        assert_eq!(p.to.to_rfc3339(), "2026-10-31T23:00:00+00:00");
        assert_eq!(p.label, "October 2026");
        assert_eq!(period(Some("garbage"), now, tz).key, "7d");
        let r = ranges(now, tz, "2026-10");
        assert_eq!(r.len(), 15);
        assert!(r[3].selected && r[3].value == "2026-10" && r[14].value == "2025-11");
    }

    #[test]
    fn numbers_and_csv_fields() {
        assert_eq!(group(0), "0");
        assert_eq!(group(999), "999");
        assert_eq!(group(12_400), "12\u{a0}400");
        assert_eq!(group(1_234_567), "1\u{a0}234\u{a0}567");
        assert_eq!(csv("plain"), "plain");
        assert_eq!(csv("a,b"), "\"a,b\"");
        assert_eq!(csv("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv("=HYPERLINK(1)"), "'=HYPERLINK(1)");
    }
}
