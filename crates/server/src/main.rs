use std::net::SocketAddr;

use anyhow::{Result, bail};
use converge_server::auth::Role;
use converge_server::config::Config;
use converge_server::{AppState, MIGRATOR, metrics_router, router, users};
use sqlx::postgres::PgPoolOptions;
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage:
  converge [serve]
  converge create-admin <email>   (then set its password)
  converge set-password <email>   (reads the password from stdin)";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    let cfg = Config::from_env()?;
    let pool = PgPoolOptions::new().max_connections(10).connect(&cfg.database_url).await?;
    MIGRATOR.run(&pool).await?;

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] | ["serve"] => {
            let state = AppState {
                cookie_secure: cfg.cookie_secure,
                public_url: cfg.public_url.clone(),
                behind_proxy: cfg.behind_proxy,
                metrics_token: cfg.metrics_token.clone(),
                ..AppState::new(pool, &cfg.secret_key)
            };
            state.start().await?;
            if let Some(bind) = &cfg.metrics_bind {
                let listener = tokio::net::TcpListener::bind(bind).await?;
                tracing::info!(bind = %bind, "metrics listening");
                let app = metrics_router(state.clone());
                tokio::spawn(async move {
                    if let Err(e) = axum::serve(listener, app).await {
                        tracing::error!(error = %e, "metrics port stopped");
                    }
                });
            }
            let usage = state.usage.clone();
            let app = router(state);
            let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
            tracing::info!(bind = %cfg.bind, "listening");
            axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
                .with_graceful_shutdown(shutdown())
                .await?;
            // What finished requests recorded goes in before the exit.
            usage.flush().await;
        }
        ["create-admin", email] => {
            let id = users::create(&pool, email, Role::Admin).await?;
            println!("created admin {email} ({id}); now run set-password {email}");
        }
        ["set-password", email] => {
            // stdin, never argv: argv lands in shell history and `ps`.
            eprint!("New password for {email}: ");
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            users::set_password(&pool, email, line.trim_end_matches(['\r', '\n'])).await?;
            println!("password set; web sessions for {email} were ended");
        }
        _ => bail!("{USAGE}"),
    }
    Ok(())
}

/// Ctrl-C, or SIGTERM from `docker stop`: in-flight requests finish first.
async fn shutdown() {
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term => {}
    }
}
