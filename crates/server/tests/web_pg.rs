//! Accounts, the web guard and every page. Needs
//! `CONVERGE_TEST_DATABASE_URL`; skipped when unset.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::upstream::{UPSTREAM_KEY, Upstream};
use common::{Browser, Db, PASSWORD, api};
use converge_server::auth::Role;

#[tokio::test]
async fn login_logout_and_security_headers() {
    let Some(db) = Db::new().await else { return };
    db.person("admin@example.com", Role::Admin).await;
    let mut b = Browser::logged_in(db.app(), "admin@example.com").await;
    let r = b.get("/settings").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("admin@example.com"));
    let csp = r.headers.get("content-security-policy").unwrap().to_str().unwrap();
    assert!(csp.contains("script-src 'self'") && !csp.contains("unsafe"), "{csp}");
    assert_eq!(r.headers.get("referrer-policy").unwrap(), "same-origin");
    let cookie = b.cookie.clone().unwrap();
    assert!(cookie.starts_with("converge_session=cvs_"), "{cookie}");

    let js = b.get("/static/htmx.min.js").await;
    let etag = js.headers.get("etag").unwrap().to_str().unwrap().to_string();
    let r304 = b.send(Request::get("/static/htmx.min.js").header("if-none-match", &etag), Body::empty(), None).await;
    assert_eq!(r304.status, StatusCode::NOT_MODIFIED);

    let r = b.post("/logout", &[]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(b.cookie.is_none());
    let r = b.get("/personas").await;
    assert_eq!(r.headers.get("location").unwrap(), "/login");
    let r = b.send(Request::get("/settings").header("hx-request", "true"), Body::empty(), None).await;
    assert_eq!(r.headers.get("hx-redirect").unwrap(), "/login");

    // The logged-out cookie no longer works even if replayed.
    b.cookie = Some(cookie);
    assert_eq!(b.get("/personas").await.status, StatusCode::SEE_OTHER);

    let r = b.post("/login", &[("email", "admin@example.com"), ("password", "wrong password!")]).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = b.post("/login", &[("email", "nobody@example.com"), ("password", PASSWORD)]).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    db.drop().await;
}

#[tokio::test]
async fn repeated_failures_block_the_account_for_a_while() {
    let Some(db) = Db::new().await else { return };
    db.person("admin@example.com", Role::Admin).await;
    let mut b = Browser::new(db.app());
    for _ in 0..converge_server::limits::PER_ACCOUNT + 1 {
        let r = b.post("/login", &[("email", "admin@example.com"), ("password", "guess guess guess")]).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    }
    let r = b.post("/login", &[("email", "Admin@Example.com"), ("password", PASSWORD)]).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS, "even the right password waits out the block");
    assert!(r.body.contains("five minutes"));
    assert!(b.cookie.is_none());
    db.drop().await;
}

#[tokio::test]
async fn cross_site_writes_are_refused() {
    let Some(db) = Db::new().await else { return };
    db.person("admin@example.com", Role::Admin).await;
    let mut b = Browser::logged_in(db.app(), "admin@example.com").await;
    for path in ["/admin/invites", "/settings/profile", "/logout"] {
        let r = b
            .send(
                Request::post(path).header("content-type", "application/x-www-form-urlencoded"),
                Body::from("role=admin&note=x&name=x&tz=UTC"),
                Some("https://evil.example"),
            )
            .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{path}");
    }
    let open: i64 = sqlx::query_scalar("SELECT count(*) FROM invites").fetch_one(&db.pool).await.unwrap();
    assert_eq!(open, 0);
    assert_eq!(b.get("/settings").await.status, StatusCode::OK, "still logged in");
    db.drop().await;
}

#[tokio::test]
async fn invites_are_single_use_and_roles_are_enforced() {
    let Some(db) = Db::new().await else { return };
    db.person("admin@example.com", Role::Admin).await;
    let mut admin = Browser::logged_in(db.app(), "admin@example.com").await;
    let r = admin.htmx_post("/admin/invites", &[("role", "user"), ("note", "Kim")]).await;
    let link = r.body.split("<code id=\"invite-link\">").nth(1).unwrap().split('<').next().unwrap().to_string();
    assert!(link.starts_with("http://test.local/invite/cvi_"), "{link}");
    let path = link.trim_start_matches("http://test.local");

    let mut kim = Browser::new(db.app());
    let form = [
        ("email", "kim@example.com"),
        ("name", "Kim"),
        ("password", "a long password"),
        ("password2", "a long password"),
    ];
    // An address that is taken keeps the invite usable.
    let taken = [
        ("email", "admin@example.com"),
        ("name", "x"),
        ("password", "a long password"),
        ("password2", "a long password"),
    ];
    assert!(kim.post(path, &taken).await.body.contains("already exists"));
    let r = kim.post(path, &form).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(kim.get("/personas").await.status, StatusCode::OK);
    let mut eve = Browser::new(db.app());
    assert_eq!(eve.get(path).await.status, StatusCode::NOT_FOUND);
    assert_eq!(eve.get("/invite/cvi_garbage").await.status, StatusCode::NOT_FOUND);
    assert_eq!(kim.get("/admin/people").await.status, StatusCode::FORBIDDEN);
    assert_eq!(kim.post("/admin/invites", &[("role", "admin"), ("note", "")]).await.status, StatusCode::FORBIDDEN);

    let kim_id: uuid::Uuid =
        sqlx::query_scalar("SELECT id FROM users WHERE email = 'kim@example.com'").fetch_one(&db.pool).await.unwrap();
    let kind: String =
        sqlx::query_scalar("SELECT kind FROM principals WHERE id = $1").bind(kim_id).fetch_one(&db.pool).await.unwrap();
    assert_eq!(kind, "user", "every user is a principal with the same id");

    admin.post(&format!("/admin/people/{kim_id}/disable"), &[]).await;
    assert_eq!(kim.get("/personas").await.status, StatusCode::SEE_OTHER);
    let mut again = Browser::new(db.app());
    let r = again.post("/login", &[("email", "kim@example.com"), ("password", "a long password")]).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED, "disabled people cannot log in");
    db.drop().await;
}

#[tokio::test]
async fn the_last_admin_cannot_step_down_or_disable_themselves() {
    let Some(db) = Db::new().await else { return };
    let me = db.person("admin@example.com", Role::Admin).await;
    let mut b = Browser::logged_in(db.app(), "admin@example.com").await;
    let r = b.htmx_post(&format!("/admin/people/{me}/role"), &[("role", "user")]).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.headers.get("hx-retarget").unwrap(), "#flash");
    let r = b.htmx_post(&format!("/admin/people/{me}/disable"), &[]).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(b.get("/admin/people").await.status, StatusCode::OK, "still an admin");
    db.drop().await;
}

/// Every page renders for an admin and a user, with data in it and no
/// inline script or style (the CSP would block them silently), and the two
/// layers stay separate pages.
#[tokio::test]
async fn every_page_renders_under_the_csp() {
    let Some(db) = Db::new().await else { return };
    let up = Upstream::start().await;
    let admin_id = db.person("admin@example.com", Role::Admin).await;
    let user_id = db.person("user@example.com", Role::User).await;
    let conn = db.connection("local", &up.base, Some(UPSTREAM_KEY)).await;
    let m = db.model(conn, "local/json", "json").await;
    db.grant(user_id, false, &[m]).await;
    let persona = db.persona(admin_id, "designer", m, "Draw.", serde_json::json!({})).await;
    let key = db.key(user_id).await;
    db.key(admin_id).await;
    let r =
        api(&db.app(), "POST", "/v1/chat/completions", Some(&key), Some(serde_json::json!({"model": "local/json"})))
            .await;
    assert_eq!(r.status, StatusCode::OK);
    db.flush().await;

    let mut admin = Browser::logged_in(db.app(), "admin@example.com").await;
    admin.htmx_post("/admin/invites", &[("role", "user"), ("note", "Sam")]).await;
    let mut user = Browser::logged_in(db.app(), "user@example.com").await;

    let pages = ["/login", "/settings", "/connections", "/personas", "/playground", "/keys", "/usage"];
    let admin_only = [
        "/admin/people".to_string(),
        "/connections/new".into(),
        format!("/connections/{conn}"),
        format!("/connections/{conn}/import"),
        format!("/admin/people/{user_id}/access"),
        "/personas/new".into(),
    ];
    for p in pages.iter().map(|p| p.to_string()).chain(admin_only.iter().cloned()) {
        let r = admin.get(&p).await;
        assert_eq!(r.status, StatusCode::OK, "{p}: {}", r.body);
        assert!(!r.body.contains("style=\"") && !r.body.contains("onclick"), "{p} has inline style or script");
        assert!(!r.body.contains(UPSTREAM_KEY), "{p} shows the upstream credential");
    }
    for p in pages {
        let r = user.get(p).await;
        assert_eq!(r.status, StatusCode::OK, "{p}: {}", r.body);
        assert!(!r.body.contains("style=\"") && !r.body.contains("onclick"), "{p} has inline style or script");
    }
    for p in &admin_only {
        assert_eq!(user.get(p).await.status, StatusCode::FORBIDDEN, "{p}");
    }
    let r = admin.get(&format!("/personas/{persona}")).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(!r.body.contains("style=\"") && !r.body.contains("onclick"));
    assert!(r.body.contains("Try it") && r.body.contains("Created on local/json"));
    converge_server::keys::set_grants(&db.pool, user_id, false, &[m], &[persona]).await.unwrap();
    db.reload().await;
    let r = user.get(&format!("/personas/{persona}")).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(!r.body.contains("style=\"") && !r.body.contains("onclick"));

    let a = admin.get("/connections").await.body;
    assert!(a.contains("<h1>Connections</h1>") && a.contains(">People</a>") && a.contains("local/json"));
    assert!(!a.contains("Personas</h1>"), "personas are not listed with connections");
    let p = admin.get("/personas").await.body;
    assert!(p.contains("<h1>Personas</h1>") && p.contains("The designer") && !p.contains("<h1>Connections</h1>"));
    let u = user.get("/connections").await.body;
    assert!(u.contains("<h1>Models</h1>") && u.contains("local/json") && !u.contains(">People</a>"));
    assert!(!u.contains(&up.base), "users do not see upstream URLs");
    let usage = user.get("/usage").await.body;
    assert!(usage.contains("11 in (2 cached) / 7 out"), "{usage}");
    let k = admin.get("/keys").await.body;
    assert!(k.contains("Everyone else") && k.contains("user@example.com"));
    assert!(!k.contains(&key), "keys are never shown again");
    assert_eq!(admin.get("/").await.headers.get("location").unwrap(), "/personas");
    db.drop().await;
}

/// The admin's whole path to a working key: add a connection, test it,
/// import from it, grant a person access; that person makes a key and uses
/// it. Every step goes through the forms.
#[tokio::test]
async fn from_connection_to_a_working_key_through_the_website() {
    let Some(db) = Db::new().await else { return };
    let up = Upstream::start().await;
    db.person("admin@example.com", Role::Admin).await;
    let kim = db.person("kim@example.com", Role::User).await;
    let mut admin = Browser::logged_in(db.app(), "admin@example.com").await;

    let base = format!("{}/", up.base);
    let form = [
        ("name", "llama-swap"),
        ("slug", "Local"),
        ("base_url", base.as_str()),
        ("credential", UPSTREAM_KEY),
        ("headers", "X-Title: Converge"),
        ("responses", "translate"),
        ("first_byte_secs", "600"),
        ("chunk_secs", "120"),
        ("enabled", "on"),
    ];
    let r = admin.post("/connections", &form).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let to = r.headers["location"].to_str().unwrap().to_string();
    let id = to.trim_start_matches("/connections/").trim_end_matches("/import").to_string();
    let bad = admin
        .htmx_post(
            "/connections",
            &[
                ("name", "x"),
                ("slug", "a/b"),
                ("base_url", "nope"),
                ("responses", "translate"),
                ("first_byte_secs", "1"),
                ("chunk_secs", "1"),
            ],
        )
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);

    let t = admin.htmx_post(&format!("/connections/{id}/test"), &[]).await;
    assert!(t.body.contains("lists 2 models"), "{}", t.body);
    let page = admin.get(&to).await.body;
    assert!(page.contains("local/beta-2") && page.contains("local/alpha"), "{page}");

    let r = admin
        .post(
            &to,
            &[("on_1", "on"), ("up_1", "Beta-2"), ("pub_1", "local/beta"), ("up_2", "alpha"), ("pub_2", "local/alpha")],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert!(admin.get(&to).await.body.contains("badge ok\">added"), "imported rows are marked");
    let dup = admin.htmx_post(&to, &[("on_2", "on"), ("up_2", "alpha"), ("pub_2", "local/beta")]).await;
    assert_eq!(dup.status, StatusCode::BAD_REQUEST, "public ids stay unique");

    // Editing with the credential left empty keeps it.
    let mut edit = form.to_vec();
    edit[3] = ("credential", "");
    edit[4] = ("headers", "");
    edit[0] = ("name", "llama-swap at home");
    assert_eq!(admin.post(&format!("/connections/{id}"), &edit).await.status, StatusCode::SEE_OTHER);
    let t = admin.htmx_post(&format!("/connections/{id}/test"), &[]).await;
    assert!(t.body.contains("lists 2 models"), "credential kept: {}", t.body);
    let form_page = admin.get(&format!("/connections/{id}")).await.body;
    assert!(form_page.contains("A key is stored") && form_page.contains("<code>x-title</code>"));

    let beta: uuid::Uuid =
        sqlx::query_scalar("SELECT id FROM models WHERE public_id = 'local/beta'").fetch_one(&db.pool).await.unwrap();
    let r = admin.post(&format!("/admin/people/{kim}/access"), &[("model", &beta.to_string())]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);

    let mut k = Browser::logged_in(db.app(), "kim@example.com").await;
    let r = k.htmx_post("/keys", &[("name", "laptop"), ("days", "30")]).await;
    let key = r.body.split("<code id=\"new-key\">").nth(1).unwrap().split('<').next().unwrap().to_string();
    assert!(key.starts_with("cvg_"));
    assert!(r.body.contains("http://test.local/v1"));
    let models = api(&db.app(), "GET", "/v1/models", Some(&key), None).await;
    let v: serde_json::Value = serde_json::from_str(&models.body).unwrap();
    assert_eq!(
        v["data"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect::<Vec<_>>(),
        ["local/beta"]
    );
    assert_eq!(v["data"][0]["owned_by"], "llama-swap at home");

    // Kim can revoke her own key, not someone else's.
    let key_id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM api_keys").fetch_one(&db.pool).await.unwrap();
    let admin_key = db.key(db.person("other@example.com", Role::User).await).await;
    let other_id: uuid::Uuid =
        sqlx::query_scalar("SELECT id FROM api_keys WHERE id <> $1").bind(key_id).fetch_one(&db.pool).await.unwrap();
    assert_eq!(k.htmx_post(&format!("/keys/{other_id}/revoke"), &[]).await.status, StatusCode::NOT_FOUND);
    assert_eq!(api(&db.app(), "GET", "/v1/models", Some(&admin_key), None).await.status, StatusCode::OK);
    assert_eq!(k.post(&format!("/keys/{key_id}/revoke"), &[]).await.status, StatusCode::SEE_OTHER);
    assert_eq!(api(&db.app(), "GET", "/v1/models", Some(&key), None).await.status, StatusCode::UNAUTHORIZED);

    // Removing the connection removes its models and their grants.
    assert_eq!(admin.post(&format!("/connections/{id}/delete"), &[]).await.status, StatusCode::SEE_OTHER);
    let grants: i64 = sqlx::query_scalar("SELECT count(*) FROM grants").fetch_one(&db.pool).await.unwrap();
    assert_eq!(grants, 0);
    db.drop().await;
}

/// M6: limits on Access, a scoped key from the form, the Usage page with
/// its period, breakdowns and limits, and the CSV.
#[tokio::test]
async fn usage_page_limits_and_scoped_keys() {
    let Some(db) = Db::new().await else { return };
    let up = Upstream::start().await;
    db.person("admin@example.com", Role::Admin).await;
    let kim = db.person("kim@example.com", Role::User).await;
    let conn = db.connection("local", &up.base, Some(UPSTREAM_KEY)).await;
    let json = db.model(conn, "local/json", "json").await;
    db.model(conn, "local/stream", "stream").await;
    db.grant(kim, true, &[]).await;
    let mut admin = Browser::logged_in(db.app(), "admin@example.com").await;
    let mut user = Browser::logged_in(db.app(), "kim@example.com").await;

    // Limits through the Access form.
    let path = format!("/admin/people/{kim}/access");
    let r = admin.post(&path, &[("all", "on"), ("rpm", "0"), ("tokens_per_day", ""), ("parallel_per_key", "")]).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "0 is not a limit");
    let r = admin
        .post(&path, &[("all", "on"), ("rpm", "60"), ("tokens_per_day", "200 000"), ("parallel_per_key", "")])
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let page = admin.get(&path).await.body;
    assert!(page.contains("name=\"rpm\" value=\"60\"") && page.contains("value=\"200000\""), "{page}");
    assert!(page.contains("name=\"parallel_per_key\" value=\"\""), "emptied: no limit");

    // A scoped key from the form.
    let keys = user.get("/keys").await.body;
    assert!(keys.contains("Only some models and personas") && keys.contains(&json.to_string()));
    let r = user
        .htmx_post("/keys", &[("name", "only json"), ("days", "0"), ("reach", "some"), ("model", &json.to_string())])
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let key = r.body.split("cvg_").nth(1).unwrap().split(['<', '"', ' ']).next().unwrap();
    let key = format!("cvg_{key}");
    let app = db.app();
    let chat = |m: &str| serde_json::json!({"model": m, "messages": [{"role": "user", "content": "hi"}]});
    assert_eq!(
        api(&app, "POST", "/v1/chat/completions", Some(&key), Some(chat("local/json"))).await.status,
        StatusCode::OK
    );
    assert_eq!(
        api(&app, "POST", "/v1/chat/completions", Some(&key), Some(chat("local/json"))).await.status,
        StatusCode::OK
    );
    assert_eq!(
        api(&app, "POST", "/v1/chat/completions", Some(&key), Some(chat("local/stream"))).await.status,
        StatusCode::NOT_FOUND
    );
    let keys = user.get("/keys").await.body;
    assert!(keys.contains("only</span> local/json"), "{keys}");

    // Usage: hers on her page, everyone's for the admin.
    db.flush().await;
    let u = user.get("/usage").await.body;
    assert!(u.contains("Last 7 days") && u.contains("By model or persona") && u.contains("By key"), "{u}");
    assert!(u.contains("<strong>2</strong>"), "two requests: {u}");
    assert!(u.contains("Tokens today: 36 of 200\u{a0}000"), "{u}");
    assert!(!u.contains("By person"), "users see only their own");
    let a = admin.get("/usage?range=today").await.body;
    assert!(a.contains("By person") && a.contains("By connection") && a.contains("local upstream"), "{a}");
    assert!(a.contains("kim@example.com"));
    let one = admin.get(&format!("/usage?range=30d&who={kim}")).await.body;
    assert!(one.contains("Last 30 days, kim@example.com"), "{one}");
    assert!(admin.get("/usage?who=nonsense").await.status.is_client_error());
    let empty = admin.get("/usage?range=2020-01").await.body;
    assert!(empty.contains("January 2020") && empty.contains("Nothing in this period"));

    let csv = user.get("/usage.csv?range=today").await;
    assert_eq!(csv.status, StatusCode::OK);
    assert!(csv.headers["content-disposition"].to_str().unwrap().starts_with("attachment; filename=\"converge-usage-"));
    let lines: Vec<&str> = csv.body.lines().collect();
    assert_eq!(lines.len(), 3, "{}", csv.body);
    assert!(lines[0].starts_with("time,person,key,model"));
    assert!(lines[1].contains(",kim@example.com,only json,local/json,,chat/completions,200,11,2,7,"), "{}", lines[1]);
    db.drop().await;
}

/// M9: editing keys through the website, by their owner and by an admin.
#[tokio::test]
async fn keys_are_edited_through_the_website() {
    let Some(db) = Db::new().await else { return };
    let up = Upstream::start().await;
    db.person("admin@example.com", Role::Admin).await;
    let kim = db.person("kim@example.com", Role::User).await;
    db.person("lee@example.com", Role::User).await;
    let conn = db.connection("local", &up.base, Some(UPSTREAM_KEY)).await;
    let json = db.model(conn, "local/json", "json").await;
    let stream = db.model(conn, "local/stream", "stream").await;
    db.grant(kim, false, &[json]).await;
    let key = db.key(kim).await;
    let id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM api_keys WHERE principal_id = $1")
        .bind(kim)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let mut kim_b = Browser::logged_in(db.app(), "kim@example.com").await;
    let mut admin = Browser::logged_in(db.app(), "admin@example.com").await;
    let mut lee_b = Browser::logged_in(db.app(), "lee@example.com").await;
    let path = format!("/keys/{id}");

    let list = kim_b.get("/keys").await.body;
    assert!(list.contains(&format!("href=\"/keys/{id}\">Edit</a>")), "{list}");
    let form = kim_b.get(&path).await.body;
    assert!(form.contains("value=\"all\" checked") && form.contains(&json.to_string()), "{form}");
    assert!(!form.contains(&stream.to_string()), "only what she may use");
    let as_admin = admin.get(&path).await.body;
    assert!(
        as_admin.contains("kim@example.com's key") && !as_admin.contains(&stream.to_string()),
        "the owner's grants, not the admin's"
    );
    assert_eq!(lee_b.get(&path).await.status, StatusCode::NOT_FOUND, "someone else's key");
    assert_eq!(
        lee_b.post(&path, &[("name", "x"), ("reach", "all"), ("days", "keep")]).await.status,
        StatusCode::NOT_FOUND
    );

    let r = kim_b.post(&path, &[("name", "x"), ("reach", "some"), ("days", "keep")]).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "only these, with nothing ticked");
    let r = kim_b
        .post(&path, &[("name", "quantifyme"), ("reach", "some"), ("model", &json.to_string()), ("days", "30")])
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let list = kim_b.get("/keys").await.body;
    assert!(list.contains("quantifyme") && list.contains("only</span> local/json"), "{list}");
    let form = kim_b.get(&path).await.body;
    assert!(form.contains("value=\"some\" checked") && form.contains(&format!("value=\"{json}\" checked")), "{form}");
    let chat = serde_json::json!({"model": "local/json", "messages": [{"role": "user", "content": "hi"}]});
    assert_eq!(
        api(&db.app(), "POST", "/v1/chat/completions", Some(&key), Some(chat)).await.status,
        StatusCode::OK,
        "same secret"
    );

    // An admin widens it back; revoked keys have no Edit and no form.
    let r = admin.post(&path, &[("name", "quantifyme"), ("reach", "all"), ("days", "keep")]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(kim_b.get("/keys").await.body.contains("everything"));
    assert_eq!(kim_b.post(&format!("{path}/revoke"), &[]).await.status, StatusCode::SEE_OTHER);
    assert!(!kim_b.get("/keys").await.body.contains(&format!("href=\"/keys/{id}\">Edit")));
    assert_eq!(kim_b.get(&path).await.status, StatusCode::NOT_FOUND);

    // The create form uses the same choice: everything unless "some".
    let r = kim_b
        .htmx_post("/keys", &[("name", "all of it"), ("days", "0"), ("reach", "all"), ("model", &json.to_string())])
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let scoped: bool =
        sqlx::query_scalar("SELECT scoped FROM api_keys WHERE name = 'all of it'").fetch_one(&db.pool).await.unwrap();
    assert!(!scoped, "ticks without \"only these\" do not narrow it");
    db.drop().await;
}
