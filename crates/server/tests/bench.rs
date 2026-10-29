//! `make bench`: time to first byte of a streaming chat request, directly
//! against the stand-in upstream and through Converge, both over real TCP on
//! this host. Prints p50 / p99 of each and of the difference.
//!
//! Ignored in normal runs; `cargo test --release --test bench -- --ignored
//! --nocapture`. Needs `CONVERGE_TEST_DATABASE_URL`. The bound is loose (p50
//! under 5 ms added) so it catches regressions without flaking on a busy CI
//! runner; real numbers go in AGENTS.md, Performance.

mod common;

use std::time::{Duration, Instant};

use common::Db;
use common::upstream::{UPSTREAM_KEY, Upstream};
use converge_server::auth::Role;
use futures_util::StreamExt;
use serde_json::json;

const RUNS: usize = 2000;
const WARMUP: usize = 100;

async fn first_byte(client: &reqwest::Client, url: &str, key: &str, model: &str) -> Duration {
    let body = json!({"model": model, "stream": true, "messages": [{"role": "user", "content": "hi"}]});
    let t = Instant::now();
    let resp = client
        .post(url)
        .bearer_auth(key)
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&body).unwrap())
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    let mut s = resp.bytes_stream();
    s.next().await.unwrap().unwrap();
    let took = t.elapsed();
    // Drain so the connection goes back to the pool for the next run.
    while s.next().await.is_some() {}
    took
}

fn pct(sorted: &[Duration], p: f64) -> Duration {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "benchmark: make bench"]
async fn added_time_to_first_byte() {
    let Some(db) = Db::new().await else { return };
    let up = Upstream::start().await;
    let user = db.person("bench@example.com", Role::User).await;
    let conn = db.connection("local", &up.base, Some(UPSTREAM_KEY)).await;
    db.model(conn, "local/bench", "bench").await;
    db.grant(user, true, &[]).await;
    let key = db.key(user).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = db.app();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let client = reqwest::Client::builder().tcp_nodelay(true).build().unwrap();
    let direct_url = format!("{}/chat/completions", up.base);
    let via_url = format!("http://{addr}/v1/chat/completions");

    for _ in 0..WARMUP {
        first_byte(&client, &direct_url, UPSTREAM_KEY, "bench").await;
        first_byte(&client, &via_url, &key, "local/bench").await;
    }
    // Interleaved, so both sides see the same load on the machine.
    let (mut direct, mut via) = (Vec::with_capacity(RUNS), Vec::with_capacity(RUNS));
    for _ in 0..RUNS {
        direct.push(first_byte(&client, &direct_url, UPSTREAM_KEY, "bench").await);
        via.push(first_byte(&client, &via_url, &key, "local/bench").await);
    }
    let mut added: Vec<Duration> = via.iter().zip(&direct).map(|(v, d)| v.saturating_sub(*d)).collect();
    direct.sort();
    via.sort();
    added.sort();
    println!("time to first byte over {RUNS} streaming requests (ms):");
    println!("  direct  p50 {:.3}  p99 {:.3}", ms(pct(&direct, 0.5)), ms(pct(&direct, 0.99)));
    println!("  via     p50 {:.3}  p99 {:.3}", ms(pct(&via, 0.5)), ms(pct(&via, 0.99)));
    println!("  added   p50 {:.3}  p99 {:.3}", ms(pct(&added, 0.5)), ms(pct(&added, 0.99)));
    db.flush().await;
    assert!(pct(&added, 0.5) < Duration::from_millis(5), "Converge adds too much at p50");
    db.drop().await;
}
