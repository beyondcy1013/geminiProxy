use std::sync::Arc;

use anyhow::Result;
use gemini_proxy::{
    config::AppConfig, local_browser_url, open_in_browser, run_server_with_state,
    terminal::manager::TerminalManager,
};
use tracing::warn;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,tower_http=warn"));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    let config = Arc::new(AppConfig::load()?);
    let terminal_manager = TerminalManager::new(&config.data_dir)?;

    let (bound_tx, bound_rx) = tokio::sync::oneshot::channel();
    if config.open_browser {
        tokio::spawn(async move {
            if let Ok(addr) = bound_rx.await {
                let url = local_browser_url(addr);
                if let Err(err) = open_in_browser(&url) {
                    warn!("failed to open browser for {url}: {err}");
                }
            }
        });
    } else {
        drop(bound_rx);
    }

    run_server_with_state(config, terminal_manager, shutdown_signal(), Some(bound_tx)).await
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
