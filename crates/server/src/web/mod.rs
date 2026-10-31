//! The website: server-rendered askama pages, enhanced with HTMX.
//!
//! Connections and Personas are separate top-level sections and never share a
//! list (AGENTS.md decision 9).
//!
//! No inline script or style anywhere (the CSP forbids both); HTMX and the
//! small `app.js` are served from here, and HTMX's own inline indicator
//! styles and eval-based features are turned off in `base.html`.

mod admin;
mod connections;
mod keys;
mod pages;
mod personas;
mod playground;
pub mod session;
mod usage;

use std::net::{IpAddr, SocketAddr};
use std::sync::LazyLock;

use axum::Router;
use axum::extract::{ConnectInfo, DefaultBodyLimit, FromRequestParts, Request};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, ETAG, HOST, IF_NONE_MATCH, ORIGIN};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use sha2::{Digest, Sha256};

use crate::AppState;
use session::WebUser;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(|| async { Redirect::to("/personas") }))
        .route("/connections", get(connections::page).post(connections::create))
        .route("/connections/new", get(connections::new_form))
        .route("/connections/{id}", get(connections::edit_form).post(connections::update))
        .route("/connections/{id}/delete", post(connections::delete))
        .route("/connections/{id}/test", post(connections::test))
        .route("/connections/{id}/import", get(connections::import_page).post(connections::import))
        .route("/connections/{id}/models", post(connections::add_model))
        .route("/models/{id}", post(connections::save_model))
        .route("/models/{id}/delete", post(connections::delete_model))
        .route("/models/{id}/params", post(connections::save_params))
        .route("/personas", get(personas::page).post(personas::create))
        .route("/personas/new", get(personas::new_form))
        .route("/personas/{id}", get(personas::show).post(personas::update))
        .route("/personas/{id}/delete", post(personas::delete))
        .route("/personas/{id}/history/{entry}/restore", post(personas::restore))
        .route("/playground", get(playground::page))
        .route("/playground/send", post(playground::send).layer(DefaultBodyLimit::max(playground::BODY_LIMIT)))
        .route("/playground/persona/{id}", get(playground::persona).post(playground::save))
        .route("/static/playground.js", get(|h: HeaderMap| asset(h, &PLAYGROUND, "text/javascript")))
        .route("/keys", get(keys::page).post(keys::create))
        .route("/keys/{id}", get(keys::edit_page).post(keys::save))
        .route("/keys/{id}/revoke", post(keys::revoke))
        .route("/usage", get(usage::page))
        .route("/usage.csv", get(usage::csv_download))
        .route("/login", get(pages::login_page).post(pages::login))
        .route("/logout", post(pages::logout))
        .route("/invite/{token}", get(pages::invite_page).post(pages::accept_invite))
        .route("/settings", get(pages::settings))
        .route("/settings/profile", post(pages::save_profile))
        .route("/settings/password", post(pages::change_password))
        .route("/admin", get(|| async { Redirect::to("/admin/people") }))
        .route("/admin/people", get(admin::people))
        .route("/admin/invites", post(admin::create_invite))
        .route("/admin/invites/{id}/delete", post(admin::delete_invite))
        .route("/admin/people/{id}/role", post(admin::set_role))
        .route("/admin/people/{id}/disable", post(admin::disable))
        .route("/admin/people/{id}/enable", post(admin::enable))
        .route("/admin/people/{id}/access", get(keys::access_page).post(keys::save_access))
        .route("/static/app.css", get(|h: HeaderMap| asset(h, &CSS, "text/css")))
        .route("/static/app.js", get(|h: HeaderMap| asset(h, &JS, "text/javascript")))
        .route("/static/htmx.min.js", get(|h: HeaderMap| asset(h, &HTMX, "text/javascript")))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(middleware::from_fn(guard))
}

/// An embedded static file with its ETag, computed once.
pub struct Asset {
    body: &'static [u8],
    etag: String,
}

fn embed(body: &'static [u8]) -> Asset {
    Asset { body, etag: format!("\"{}\"", &hex::encode(Sha256::digest(body))[..16]) }
}

static CSS: LazyLock<Asset> = LazyLock::new(|| embed(include_bytes!("../../static/app.css")));
static JS: LazyLock<Asset> = LazyLock::new(|| embed(include_bytes!("../../static/app.js")));
static PLAYGROUND: LazyLock<Asset> = LazyLock::new(|| embed(include_bytes!("../../static/playground.js")));
/// htmx 2.0.4, vendored. sha256 in AGENTS.md.
static HTMX: LazyLock<Asset> = LazyLock::new(|| embed(include_bytes!("../../static/htmx.min.js")));

/// `no-cache` + ETag: a deploy is picked up on the next page view, and every
/// other view is a 304 with no body.
async fn asset(headers: HeaderMap, a: &Asset, content_type: &'static str) -> Response {
    let etag = HeaderValue::from_str(&a.etag).expect("hex etag");
    let common = [(CACHE_CONTROL, HeaderValue::from_static("no-cache")), (ETAG, etag)];
    if headers.get(IF_NONE_MATCH).is_some_and(|v| v.as_bytes() == a.etag.as_bytes()) {
        return (StatusCode::NOT_MODIFIED, common).into_response();
    }
    (common, [(CONTENT_TYPE, HeaderValue::from_static(content_type))], a.body).into_response()
}

/// Nothing external: every byte the pages use comes from this server.
pub const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; \
     connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

/// Refuses cross-site writes (Origin must name this host when sent) and sets
/// the page security headers. SameSite=Strict cookies already stop most CSRF;
/// this is the second lock. Every method except GET/HEAD counts as a write,
/// since HTMX can send DELETE and PUT. `/v1` will not sit behind it: bearer
/// keys are not sent by browsers on their own.
async fn guard(req: Request, next: Next) -> Response {
    if !matches!(*req.method(), Method::GET | Method::HEAD)
        && let Some(origin) = req.headers().get(ORIGIN).and_then(|v| v.to_str().ok())
    {
        let host = req.headers().get(HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
        let origin_host = origin.split("://").nth(1).unwrap_or("");
        if origin_host != host {
            return (StatusCode::FORBIDDEN, "cross-site request refused").into_response();
        }
    }
    let htmx = is_htmx(req.headers());
    let mut resp = next.run(req).await;
    if !htmx && resp.headers().contains_key("hx-retarget") {
        resp = error_page(resp).await;
    }
    let h = resp.headers_mut();
    h.insert("content-security-policy", HeaderValue::from_static(CSP));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    // Not `no-referrer`: under it browsers send `Origin: null` on every form
    // POST (Fetch spec), which the check above refuses. `same-origin` keeps
    // the real Origin on our own posts and still sends nothing cross-site.
    h.insert("referrer-policy", HeaderValue::from_static("same-origin"));
    resp
}

/// An error fragment (see `WebError`) reached a plain browser request (no
/// JavaScript): wrap it in a minimal page with a way back.
async fn error_page(resp: Response) -> Response {
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap_or_default();
    let fragment = String::from_utf8_lossy(&bytes);
    let page = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" \
         content=\"width=device-width, initial-scale=1\"><title>Converge</title><link rel=\"stylesheet\" \
         href=\"/static/app.css\"></head><body><main class=\"narrow\"><section class=\"card\">{fragment}\
         <p>Use your browser's back button, or <a href=\"/\">start over</a>.</p></section></main></body></html>"
    );
    (status, axum::response::Html(page)).into_response()
}

/// What every logged-in page needs for the header.
pub struct Nav {
    pub label: String,
    pub admin: bool,
    pub active: &'static str,
}

impl Nav {
    pub fn new(u: &WebUser, active: &'static str) -> Self {
        Nav { label: u.label().to_string(), admin: u.is_admin(), active }
    }

    /// `class="active"` for the current section's link.
    pub fn on(&self, section: &str) -> &'static str {
        if self.active == section { "active" } else { "" }
    }
}

pub fn html(t: impl askama::Template) -> Result<Response, crate::error::WebError> {
    t.render()
        .map(|s| axum::response::Html(s).into_response())
        .map_err(|e| crate::error::ApiError::Internal(anyhow::anyhow!("template: {e}")).into())
}

/// Whether the request came from HTMX (fragment) or a full page load.
pub fn is_htmx(headers: &HeaderMap) -> bool {
    headers.contains_key("hx-request")
}

/// This server's own base URL as the browser sees it (invite links).
pub fn base_url(headers: &HeaderMap, secure: bool) -> String {
    let host = headers.get(HOST).and_then(|v| v.to_str().ok()).unwrap_or("localhost");
    format!("{}://{host}", if secure { "https" } else { "http" })
}

/// A date in the viewer's time zone.
pub fn local_day(t: chrono::DateTime<chrono::Utc>, tz: chrono_tz::Tz) -> String {
    t.with_timezone(&tz).format("%Y-%m-%d").to_string()
}

/// The client's address, for login throttling. Behind a proxy (declared with
/// `CONVERGE_BEHIND_PROXY`) it is the right-most `X-Forwarded-For` entry: the
/// one the proxy itself appended, which a client cannot forge. Otherwise the
/// socket peer. None in tests, which have neither.
pub struct ClientAddr(pub Option<IpAddr>);

impl FromRequestParts<AppState> for ClientAddr {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Self::Rejection> {
        let peer = parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip());
        if !st.behind_proxy {
            return Ok(ClientAddr(peer));
        }
        Ok(ClientAddr(forwarded_for(&parts.headers).or(peer)))
    }
}

fn forwarded_for(headers: &HeaderMap) -> Option<IpAddr> {
    // Several headers are one list, in order; the last entry of the last one
    // is what the nearest proxy added.
    headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .next_back()
        .and_then(|s| s.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwarded_for_takes_the_proxys_entry() {
        let mut h = HeaderMap::new();
        assert_eq!(forwarded_for(&h), None);
        h.append("x-forwarded-for", HeaderValue::from_static("6.6.6.6, 203.0.113.7"));
        assert_eq!(forwarded_for(&h), "203.0.113.7".parse().ok());
        h.append("x-forwarded-for", HeaderValue::from_static("198.51.100.2"));
        assert_eq!(forwarded_for(&h), "198.51.100.2".parse().ok());
        h.append("x-forwarded-for", HeaderValue::from_static("not-an-ip"));
        assert_eq!(forwarded_for(&h), None, "garbage counts as unknown, not as the forged entry");
    }
}
