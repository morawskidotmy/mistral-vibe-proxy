mod handler;
mod key;
mod models;
mod overflow;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::routing::{any, get};
use axum::Router;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::handler::{build_client, AppState};
use crate::models::ModelRegistry;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "vibe_proxy=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let host = std::env::var("VIBE_PROXY_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port: u16 = std::env::var("VIBE_PROXY_PORT")
        .unwrap_or_else(|_| "6933".to_string())
        .parse()
        .expect("Valid port number");
    let timeout = Duration::from_secs(
        std::env::var("VIBE_PROXY_TIMEOUT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(600),
    );
    let api_base = std::env::var("VIBE_PROXY_API_BASE")
        .ok()
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| "https://api.mistral.ai/v1".to_string());
    let proxy_api_key = std::env::var("VIBE_PROXY_API_KEY")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let model_registry = ModelRegistry::load();
    let state = Arc::new(AppState {
        client: build_client(timeout),
        api_base,
        proxy_api_key,
        models: model_registry,
    });

    info!(
        "Starting Vibe OpenAI proxy on {}:{} -> {} (auth: {}, timeout: {}s)",
        host,
        port,
        state.api_base,
        if state.proxy_api_key.is_some() { "on" } else { "off" },
        timeout.as_secs()
    );

    let app = Router::new()
        .route("/", get(handler::health))
        .route("/healthz", get(handler::health))
        .route("/health", get(handler::health))
        .route("/v1/models", get(handler::models_route))
        .route("/models", get(handler::models_route))
        // Everything else is forwarded to the Mistral API verbatim,
        // including POST /v1/chat/completions (streaming and tools).
        .fallback(any(handler::proxy_any))
        .with_state(state);

    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("Vibe proxy listening on http://{addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("Vibe proxy server stopped.");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
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
