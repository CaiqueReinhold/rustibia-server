mod api;
mod auth;
mod config;
mod db;
mod domain;
mod error;
mod internal_tls;
mod state;
mod template;
mod web;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use axum::{Router, routing::get};
use sqlx::postgres::PgPoolOptions;
use tower_governor::{
    GovernorLayer, governor::GovernorConfigBuilder, key_extractor::SmartIpKeyExtractor,
};
use tracing::info;

use crate::{config::SiteConfig, state::AppState};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let config = SiteConfig::load("config.yaml").context("loading config.yaml")?;

    let database_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://rustibia:rustibia@localhost:5432/rustibia".to_string());
    let bind_address =
        std::env::var("BIND_ADDRESS").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    let internal_bind_address =
        std::env::var("INTERNAL_BIND_ADDRESS").unwrap_or_else(|_| "127.0.0.1:8443".to_string());

    let internal_tls = internal_tls::server_config(&internal_tls::InternalTlsPaths::from_env())
        .context(
            "building the internal mTLS configuration — run `cargo run -p rustibia-certgen` \
             to generate certs/, or point INTERNAL_TLS_CERT/_KEY/_CLIENT_CA at existing ones",
        )?;

    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&database_url)
        .await
        .context("connecting to Postgres")?;

    sqlx::migrate!()
        .run(&pool)
        .await
        .context("running migrations")?;
    info!("migrations applied");

    let state = AppState { pool, config };

    // 5 requests burst, refilling one every 2 seconds, keyed by client IP.
    // `X-Forwarded-For` is trusted: in production only nginx can reach this listener, and
    // it overwrites the header rather than appending (deploy/ansible/templates/nginx.conf.j2).
    let governor_config = Arc::new(
        GovernorConfigBuilder::default()
            .key_extractor(SmartIpKeyExtractor)
            .per_second(2)
            .burst_size(5)
            .finish()
            .expect("valid governor configuration"),
    );

    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .merge(web::credential_router().layer(GovernorLayer {
            config: governor_config.clone(),
        }))
        .merge(web::account_router())
        .merge(web::public_router())
        .nest(
            "/api",
            api::router().layer(GovernorLayer {
                config: governor_config,
            }),
        )
        .nest_service("/static", tower_http::services::ServeDir::new("static"))
        .with_state(state.clone());

    let internal_app = Router::new()
        .nest("/internal", api::internal::router())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&bind_address).await?;
    info!("listening on {bind_address}");

    let internal_listener = std::net::TcpListener::bind(&internal_bind_address)
        .with_context(|| format!("binding the internal listener on {internal_bind_address}"))?;
    internal_listener.set_nonblocking(true)?;
    info!("internal mTLS listener on {internal_bind_address}");

    let public = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .context("the public listener stopped")
    });

    let internal = tokio::spawn(async move {
        axum_server::from_tcp_rustls(
            internal_listener,
            axum_server::tls_rustls::RustlsConfig::from_config(internal_tls),
        )
        .context("adopting the internal listener socket")?
        .serve(internal_app.into_make_service())
        .await
        .context("the internal listener stopped")
    });

    // Either listener stopping is fatal. Serving the website while logins are refused,
    // or accepting logins with no website to mint tokens, are both worse than exiting
    // and letting the supervisor restart the process.
    tokio::select! {
        result = public => result.context("the public listener panicked")?,
        result = internal => result.context("the internal listener panicked")?,
    }
}
