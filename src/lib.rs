//! concord-mcp: an MCP hub where AI agents of different people share one plan, split the
//! work, file tasks and bugs into each other's areas, and always reach a decision when they
//! disagree.

pub mod access;
pub mod api;
pub mod clock;
pub mod connectors;
pub mod error;
pub mod glob;
pub mod http;
pub mod hub;
pub mod invite;
pub mod mcp;
pub mod model;
pub mod notify;
pub mod ops;
pub mod policy;
pub mod session_store;
pub mod sweeper;
pub mod token;
pub mod ui;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use tokio_util::sync::CancellationToken;

pub use error::{HubError, Result};
pub use hub::Hub;

#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub listen: SocketAddr,
    pub http: http::HttpConfig,
    pub sweep_every: Duration,
}

pub async fn connect(database_url: &str, max_connections: u32) -> anyhow::Result<sqlx::PgPool> {
    Ok(PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(Duration::from_secs(10))
        .connect(database_url)
        .await?)
}

/// Runs the hub until `cancel` fires: HTTP, Postgres listener, sweeper.
pub async fn serve(hub: Arc<Hub>, opts: ServeOptions, cancel: CancellationToken) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(opts.listen).await?;
    tracing::info!(addr = %listener.local_addr()?, "concord hub listening");
    let listen_task = tokio::spawn(notify::listen(hub.pool.clone(), hub.notifier.clone(), cancel.child_token()));
    let sweep_task = tokio::spawn(sweeper::run(hub.clone(), opts.sweep_every, cancel.child_token()));
    let connector_task = tokio::spawn(connectors::engine::run(hub.clone(), opts.sweep_every, cancel.child_token()));
    let app = http::router(hub, opts.http, cancel.child_token());
    let shutdown = cancel.clone();
    axum::serve(listener, app).with_graceful_shutdown(async move { shutdown.cancelled().await }).await?;
    cancel.cancel();
    let _ = tokio::join!(listen_task, sweep_task, connector_task);
    Ok(())
}
