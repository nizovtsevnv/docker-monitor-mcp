//! `docker-monitor-mcp` — stateless read-only MCP server exposing Docker
//! container logs and host/container metrics.
//!
//! Transport — Streamable HTTP (JSON-RPC 2.0). Tools: `docker_logs`,
//! `host_metrics`, `container_metrics`. See `docs/SPEC.md`.

mod config;
mod docker;
mod mcp;
mod metrics;

use std::sync::Arc;

use config::Config;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Logs go to stderr; level controlled by RUST_LOG (default info).
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let config = Config::from_env();
    info!(
        bind = %config.bind_addr,
        docker_socket = %config.docker_socket,
        proc = %config.proc_path,
        "starting docker-monitor-mcp"
    );

    let docker = docker::connect(&config.docker_socket)?;

    let state = Arc::new(mcp::AppState {
        docker,
        config: config.clone(),
    });
    let app = mcp::router(state);

    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
    info!(addr = %config.bind_addr, "MCP HTTP listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("stopped");
    Ok(())
}

/// Waits for SIGINT/SIGTERM for graceful shutdown (matters for Swarm rolling updates).
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
