pub mod admin;
pub mod llm;

use std::sync::Arc;

use axum::{
    Router,
    routing::{delete, get, post},
};
use tower_http::cors::{Any, CorsLayer};

use crate::{config::AppConfig, terminal::manager::TerminalManager};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub terminal_manager: Arc<TerminalManager>,
}

pub fn build_router(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .route("/health", get(admin::health))
        .route("/", get(admin::index_html))
        .route("/index.html", get(admin::index_html))
        .route("/style.css", get(admin::style_css))
        .route("/app.js", get(admin::app_js))
        .route("/vendor/xterm.js", get(admin::xterm_js))
        .route("/vendor/xterm.css", get(admin::xterm_css))
        .route("/vendor/xterm-addon-fit.js", get(admin::xterm_addon_fit_js))
        .route("/api/info", get(admin::get_info))
        .route("/api/models", get(admin::list_model_catalog).post(admin::upsert_model_catalog))
        .route("/api/models/{id}", delete(admin::delete_model_catalog))
        .route("/api/keys", post(admin::upsert_api_key))
        .route("/api/sessions", get(admin::list_sessions).post(admin::create_session))
        .route("/api/sessions/{id}", delete(admin::delete_session))
        .route("/api/sessions/{id}/input", post(admin::send_session_input))
        .route("/api/sessions/{id}/snapshot", get(admin::get_session_snapshot))
        .route("/ws/terminal/{id}", get(admin::ws_terminal))
        .route("/v1/models", get(llm::list_models))
        .route("/models", get(llm::list_models))
        .route("/v1/chat/completions", post(llm::chat_completions))
        .route("/chat/completions", post(llm::chat_completions))
        .route("/v1/responses", post(llm::responses))
        .route("/responses", post(llm::responses))
        .layer(cors)
        .with_state(state)
}
