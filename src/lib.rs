pub mod config;
pub mod routes;
pub mod terminal;

use std::{future::Future, net::SocketAddr, sync::Arc, time::Duration};

use anyhow::Result;
use tokio::net::TcpListener;
use tracing::info;

use config::AppConfig;
use routes::{AppState, build_router};
use terminal::manager::TerminalManager;

pub async fn run_server_with_state<F>(
    config: Arc<AppConfig>,
    terminal_manager: Arc<TerminalManager>,
    shutdown: F,
    bound_tx: Option<tokio::sync::oneshot::Sender<SocketAddr>>,
) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let reaper_config = config.clone();
    let reaper_manager = terminal_manager.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            let models = reaper_config.list_models();
            let mgr = reaper_manager.clone();
            let _ = tokio::task::spawn_blocking(move || mgr.reap_expired_managed_sessions(&models))
                .await;
        }
    });

    let state = AppState {
        config: config.clone(),
        terminal_manager,
    };

    let app = build_router(state);
    let listener = TcpListener::bind(config.listen_addr).await?;
    let actual_addr = listener.local_addr()?;

    if let Some(tx) = bound_tx {
        let _ = tx.send(actual_addr);
    }

    info!("gemini-proxy listening on http://{actual_addr}");
    info!("config file: {}", config.config_path.display());
    if let Some(primary_key) = config.primary_api_key_secret() {
        info!("primary API key: {primary_key}");
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;

    info!("gemini-proxy stopped");
    Ok(())
}

pub fn local_browser_url(addr: SocketAddr) -> String {
    let host = match addr.ip() {
        ip if ip.is_unspecified() => "127.0.0.1".to_string(),
        ip => ip.to_string(),
    };
    format!("http://{host}:{}", addr.port())
}

pub fn open_in_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()?;
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()?;
        return Ok(());
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
        return Ok(());
    }
}
