use std::path::PathBuf;

use axum::{
    Json,
    extract::{Path, Query, State, WebSocketUpgrade},
    http::{HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Response},
};
use serde::Deserialize;

use super::AppState;
use crate::{
    config::{ApiKeyPolicy, ModelSpec},
    terminal::{
        manager::CreateSessionParams,
        session::{AgentKind, capture_session_plain, send_session_paste},
        websocket::handle_terminal_socket,
    },
};

const INDEX_HTML: &str = include_str!("../../static/index.html");
const STYLE_CSS: &str = include_str!("../../static/style.css");
const APP_JS: &str = include_str!("../../static/app.js");
const XTERM_JS: &str = include_str!("../../static/vendor/xterm.js");
const XTERM_CSS: &str = include_str!("../../static/vendor/xterm.css");
const XTERM_ADDON_FIT_JS: &str = include_str!("../../static/vendor/xterm-addon-fit.js");

pub async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true, "service": "gemini-proxy" }))
}

pub async fn index_html() -> Html<&'static str> {
    Html(INDEX_HTML)
}

pub async fn style_css() -> Response {
    asset_response("text/css; charset=utf-8", STYLE_CSS)
}

pub async fn app_js() -> Response {
    asset_response("application/javascript; charset=utf-8", APP_JS)
}

pub async fn xterm_js() -> Response {
    asset_response("application/javascript; charset=utf-8", XTERM_JS)
}

pub async fn xterm_css() -> Response {
    asset_response("text/css; charset=utf-8", XTERM_CSS)
}

pub async fn xterm_addon_fit_js() -> Response {
    asset_response("application/javascript; charset=utf-8", XTERM_ADDON_FIT_JS)
}

fn asset_response(content_type: &'static str, body: &'static str) -> Response {
    let mut resp = body.into_response();
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    resp
}

pub async fn get_info(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "listen_addr": state.config.listen_addr.to_string(),
        "api_key_masked": state.config.primary_api_key_masked(),
        "api_key_count": state.config.api_key_count(),
        "api_keys": state.config.api_key_summaries(),
        "models": state.config.list_models(),
        "request_timeout_secs": state.config.request_timeout_secs,
        "idle_quiesce_ms": state.config.idle_quiesce_ms,
        "config_path": state.config.config_path,
    }))
}

pub async fn list_model_catalog(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "models": state.config.list_models() }))
}

pub async fn upsert_model_catalog(
    State(state): State<AppState>,
    Json(spec): Json<ModelSpec>,
) -> Response {
    match state.config.upsert_model(spec) {
        Ok(saved) => Json(serde_json::json!({ "model": saved })).into_response(),
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("{err:#}") })),
        )
            .into_response(),
    }
}

pub async fn delete_model_catalog(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    match state.config.delete_model(&id) {
        Ok(true) => Json(serde_json::json!({ "deleted": true, "id": id })).into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "model not found" })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("{err:#}") })),
        )
            .into_response(),
    }
}

pub async fn upsert_api_key(
    State(state): State<AppState>,
    Json(policy): Json<ApiKeyPolicy>,
) -> Response {
    match state.config.upsert_api_key_policy(policy) {
        Ok(saved) => Json(serde_json::json!({
            "id": saved.id,
            "name": saved.name,
            "allowed_models": saved.allowed_models,
        }))
        .into_response(),
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("{err:#}") })),
        )
            .into_response(),
    }
}

pub async fn list_sessions(State(state): State<AppState>) -> Json<serde_json::Value> {
    let sessions = state.terminal_manager.list_session_views();
    Json(serde_json::json!({ "sessions": sessions }))
}

#[derive(Debug, Deserialize)]
pub struct CreateSessionBody {
    pub name: String,
    #[serde(default)]
    pub model_alias: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub startup_command: Option<String>,
    #[serde(default)]
    pub agent_kind: AgentKind,
    #[serde(default)]
    pub use_markers: bool,
}

pub async fn create_session(
    State(state): State<AppState>,
    Json(body): Json<CreateSessionBody>,
) -> Response {
    let params = CreateSessionParams {
        name: body.name,
        model_alias: body.model_alias,
        cwd: body
            .cwd
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from),
        startup_command: body.startup_command,
        agent_kind: body.agent_kind,
        use_markers: body.use_markers,
        owner_key_id: None,
        managed: false,
        explicit_conversation_id: None,
    };

    let manager = state.terminal_manager.clone();
    match tokio::task::spawn_blocking(move || manager.create_session(params)).await {
        Ok(Ok(session)) => {
            let views = state.terminal_manager.list_session_views();
            let created = views.into_iter().find(|v| v.id == session.id);
            (
                StatusCode::CREATED,
                Json(serde_json::json!({ "session": created })),
            )
                .into_response()
        }
        Ok(Err(err)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("{err:#}") })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("create task failed: {err}") })),
        )
            .into_response(),
    }
}

pub async fn delete_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let manager = state.terminal_manager.clone();
    let sid = id.clone();
    match tokio::task::spawn_blocking(move || manager.delete_session(&sid)).await {
        Ok(Ok(true)) => Json(serde_json::json!({ "deleted": true, "id": id })).into_response(),
        Ok(Ok(false)) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "session not found" })),
        )
            .into_response(),
        Ok(Err(err)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("{err:#}") })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("delete task failed: {err}") })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct SendInputBody {
    pub text: String,
    #[serde(default = "default_true")]
    pub send_enter: bool,
}

fn default_true() -> bool {
    true
}

pub async fn send_session_input(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SendInputBody>,
) -> Response {
    let Some(session) = state.terminal_manager.get_session(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "session not found" })),
        )
            .into_response();
    };

    let text = body.text;
    let send_enter = body.send_enter;
    match tokio::task::spawn_blocking(move || send_session_paste(&session, &text, send_enter)).await
    {
        Ok(Ok(())) => Json(serde_json::json!({ "ok": true })).into_response(),
        Ok(Err(err)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("{err:#}") })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("input task failed: {err}") })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct SnapshotQuery {
    #[serde(default = "default_snapshot_lines")]
    pub lines: u32,
}

fn default_snapshot_lines() -> u32 {
    200
}

pub async fn get_session_snapshot(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<SnapshotQuery>,
) -> Response {
    let Some(session) = state.terminal_manager.get_session(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "session not found" })),
        )
            .into_response();
    };

    let lines = query.lines.clamp(20, 5000);
    match tokio::task::spawn_blocking(move || capture_session_plain(&session, lines)).await {
        Ok(Ok(text)) => Json(serde_json::json!({ "session_id": id, "text": text })).into_response(),
        Ok(Err(err)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("{err:#}") })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("snapshot task failed: {err}") })),
        )
            .into_response(),
    }
}

pub async fn ws_terminal(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let Some(session) = state.terminal_manager.get_session(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "session not found" })),
        )
            .into_response();
    };
    ws.on_upgrade(move |socket| handle_terminal_socket(socket, session))
}
