use std::{convert::Infallible, sync::Arc, time::Duration};

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use futures_util::stream;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{error, info, warn};

use super::AppState;
use crate::{
    config::{AuthenticatedKey, ModelSpec},
    terminal::{
        account_rotation::{current_active_codebuddy_account_id, rotate_codebuddy_account},
        agent::{
            AgentTurnOutcome, ChatMessage, DetectedAgent, MessageContent, canonical_messages_hash,
            execute_agent_turn, pane_has_quota_exhausted,
        },
        manager::{AcquireError, TurnMode},
        session::{TerminalSession, now_millis},
    },
};

const MAX_ACCOUNT_ROTATIONS: usize = 2;

#[derive(Debug, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct ResponsesRequest {
    pub model: String,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub previous_response_id: Option<String>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Serialize)]
struct OpenAiErrorEnvelope {
    error: OpenAiErrorBody,
}

#[derive(Debug, Serialize)]
struct OpenAiErrorBody {
    message: String,
    #[serde(rename = "type")]
    error_type: &'static str,
    code: &'static str,
}

fn openai_error(
    status: StatusCode,
    message: impl Into<String>,
    error_type: &'static str,
    code: &'static str,
) -> Response {
    (
        status,
        Json(OpenAiErrorEnvelope {
            error: OpenAiErrorBody {
                message: message.into(),
                error_type,
                code,
            },
        }),
    )
        .into_response()
}

fn map_acquire_error(err: AcquireError) -> Response {
    match err {
        AcquireError::Forbidden(msg) => {
            openai_error(StatusCode::FORBIDDEN, msg, "invalid_request_error", "model_not_allowed")
        }
        AcquireError::ModelNotFound(msg) => {
            openai_error(StatusCode::NOT_FOUND, msg, "invalid_request_error", "model_not_found")
        }
        AcquireError::PreviousResponseNotFound(msg) => openai_error(
            StatusCode::NOT_FOUND,
            msg,
            "invalid_request_error",
            "previous_response_not_found",
        ),
        AcquireError::TooManyBusySessions(msg) => openai_error(
            StatusCode::TOO_MANY_REQUESTS,
            msg,
            "rate_limit_error",
            "too_many_busy_sessions",
        ),
        AcquireError::Internal(err) => openai_error(
            StatusCode::BAD_GATEWAY,
            format!("{err:#}"),
            "server_error",
            "launch_failed",
        ),
    }
}

struct TurnExecutionResult {
    session: Arc<TerminalSession>,
    mode: TurnMode,
    outcome: AgentTurnOutcome,
    rotated_account_id: Option<String>,
}

async fn run_turn_with_model_and_rotation(
    state: &AppState,
    auth_key: &AuthenticatedKey,
    model_spec: Option<&ModelSpec>,
    full_messages: &[ChatMessage],
    mut session: Arc<TerminalSession>,
    mut mode: TurnMode,
    mut messages_to_send: Vec<ChatMessage>,
    explicit_conv_id: Option<&str>,
    timeout: Duration,
    idle_quiesce: Duration,
    req_id: &str,
) -> std::result::Result<TurnExecutionResult, Response> {
    let desired_model = model_spec.and_then(|s| s.upstream_model.as_deref());
    let mut tried_account_ids: Vec<String> = Vec::new();
    let mut rotated_account_id: Option<String> = None;

    for attempt in 0..=MAX_ACCOUNT_ROTATIONS {
        let allow_switch_when_unknown = mode == TurnMode::Continue || !session.managed;
        let outcome = match execute_agent_turn(
            session.clone(),
            &messages_to_send,
            desired_model,
            allow_switch_when_unknown,
            timeout,
            idle_quiesce,
            format!("{req_id}-a{attempt}"),
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(err) => {
                error!(request_id = %req_id, session_id = %session.id, "agent turn failed: {err:#}");
                let status = if err.to_string().contains("timed out") {
                    StatusCode::GATEWAY_TIMEOUT
                } else {
                    StatusCode::BAD_GATEWAY
                };
                return Err(openai_error(
                    status,
                    format!("{err:#}"),
                    "server_error",
                    "agent_error",
                ));
            }
        };

        let quota_hit = pane_has_quota_exhausted(&outcome.reply)
            .or_else(|| pane_has_quota_exhausted(&outcome.pane_tail));

        if let Some(marker) = quota_hit {
            if outcome.detected_agent == DetectedAgent::CodeBuddy && attempt < MAX_ACCOUNT_ROTATIONS
            {
                if let Some(spec) = model_spec {
                    let tried_snapshot = tried_account_ids.clone();
                    let rotate_res = tokio::task::spawn_blocking(move || {
                        let mut tried = tried_snapshot;
                        if let Some(active_id) = current_active_codebuddy_account_id() {
                            if !tried.contains(&active_id) {
                                tried.push(active_id);
                            }
                        }
                        let rotated = rotate_codebuddy_account(&tried)?;
                        Ok::<_, anyhow::Error>((tried, rotated))
                    })
                    .await;

                    if let Ok(Ok((new_tried, Some(rotated)))) = rotate_res {
                        tried_account_ids = new_tried;
                        tried_account_ids.push(rotated.account_id.clone());
                        warn!(
                            request_id = %req_id,
                            session_id = %session.id,
                            marker,
                            new_account = %rotated.account_id,
                            "quota exhausted detected; rotated CodeBuddy account and respawning tmux session"
                        );
                        let mgr = state.terminal_manager.clone();
                        let old_sid = session.id.clone();
                        let auth_clone = auth_key.clone();
                        let spec_clone = spec.clone();
                        let conv_clone = explicit_conv_id.map(str::to_string);
                        match tokio::task::spawn_blocking(move || {
                            mgr.replace_exhausted_session(
                                &old_sid,
                                &auth_clone,
                                &spec_clone,
                                conv_clone.as_deref(),
                            )
                        })
                        .await
                        {
                            Ok(Ok(fresh_session)) => {
                                session = fresh_session;
                                mode = TurnMode::New;
                                messages_to_send = full_messages.to_vec();
                                rotated_account_id = Some(rotated.account_id);
                                continue;
                            }
                            Ok(Err(err)) => return Err(map_acquire_error(err)),
                            Err(err) => {
                                return Err(openai_error(
                                    StatusCode::BAD_GATEWAY,
                                    format!("failed to respawn session after rotation: {err}"),
                                    "server_error",
                                    "rotation_respawn_failed",
                                ));
                            }
                        }
                    }
                }
            }
        }

        return Ok(TurnExecutionResult {
            session,
            mode,
            outcome,
            rotated_account_id,
        });
    }

    Err(openai_error(
        StatusCode::TOO_MANY_REQUESTS,
        "all CodeBuddy accounts exhausted quota",
        "rate_limit_error",
        "quota_exhausted",
    ))
}

pub async fn list_models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(auth_key) = state.config.authenticate_headers(&headers) else {
        return openai_error(
            StatusCode::UNAUTHORIZED,
            "Invalid or missing API key",
            "invalid_request_error",
            "invalid_api_key",
        );
    };

    let created = now_millis() / 1000;
    let mut seen = std::collections::HashSet::new();
    let mut data: Vec<Value> = Vec::new();

    for spec in state.config.list_models() {
        if !auth_key.can_access_model(&spec.id) {
            continue;
        }
        if seen.insert(spec.id.clone()) {
            data.push(serde_json::json!({
                "id": spec.id,
                "object": "model",
                "created": created,
                "owned_by": "gemini-proxy",
                "upstream_model": spec.upstream_model,
                "launch_command": spec.launch_command,
                "max_sessions": spec.max_sessions,
            }));
        }
    }

    for s in state.terminal_manager.list_sessions() {
        if !auth_key.can_access_model(&s.model_alias) {
            continue;
        }
        if let Some(ref owner) = s.owner_key_id {
            if owner != &auth_key.id {
                continue;
            }
        }
        if seen.insert(s.model_alias.clone()) {
            data.push(serde_json::json!({
                "id": s.model_alias,
                "object": "model",
                "created": created,
                "owned_by": "gemini-proxy",
                "session_id": s.id,
            }));
        }
    }

    Json(serde_json::json!({
        "object": "list",
        "data": data,
    }))
    .into_response()
}

pub async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ChatCompletionRequest>,
) -> Response {
    let Some(auth_key) = state.config.authenticate_headers(&headers) else {
        return openai_error(
            StatusCode::UNAUTHORIZED,
            "Invalid or missing API key",
            "invalid_request_error",
            "invalid_api_key",
        );
    };

    if req.messages.is_empty() {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "messages must be a non-empty array",
            "invalid_request_error",
            "invalid_messages",
        );
    }

    let explicit_conv_id = headers
        .get("x-conversation-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let model_spec = state.config.find_model(&req.model);
    let acquired = match state.terminal_manager.acquire_session_for_turn(
        &auth_key,
        &req.model,
        model_spec.as_ref(),
        &req.messages,
        None,
        explicit_conv_id.as_deref(),
        state.config.default_session_id.as_deref(),
    ) {
        Ok(acq) => acq,
        Err(err) => return map_acquire_error(err),
    };

    let req_id = generate_id("chatcmpl-");
    let timeout = Duration::from_secs(
        req.timeout_secs
            .unwrap_or(state.config.request_timeout_secs)
            .clamp(5, 3600),
    );
    let idle_quiesce = Duration::from_millis(state.config.idle_quiesce_ms);

    info!(
        request_id = %req_id,
        key_id = %auth_key.id,
        session_id = %acquired.session.id,
        model = %req.model,
        turn_mode = acquired.mode.as_str(),
        stream = req.stream,
        endpoint = "chat_completions",
        "dispatching LLM API request to tmux terminal agent"
    );

    let exec = match run_turn_with_model_and_rotation(
        &state,
        &auth_key,
        model_spec.as_ref(),
        &req.messages,
        acquired.session,
        acquired.mode,
        acquired.messages_to_send,
        explicit_conv_id.as_deref(),
        timeout,
        idle_quiesce,
        &req_id,
    )
    .await
    {
        Ok(res) => res,
        Err(err_resp) => return err_resp,
    };

    let mut full_after_turn = req.messages.clone();
    full_after_turn.push(ChatMessage {
        role: "assistant".to_string(),
        content: MessageContent::Text(exec.outcome.reply.clone()),
    });
    let new_hash = canonical_messages_hash(&full_after_turn);
    exec.session
        .record_turn(new_hash, None, explicit_conv_id);
    let _ = state.terminal_manager.persist_sessions();

    let created = now_millis() / 1000;
    let prompt_tokens = estimate_prompt_tokens(&req.messages);
    let completion_tokens = estimate_tokens(&exec.outcome.reply);

    let mut resp = if req.stream {
        let chunks = build_sse_chunks(&req_id, created, &req.model, &exec.outcome.reply);
        let sse_stream = stream::iter(
            chunks
                .into_iter()
                .map(|payload| Ok::<Event, Infallible>(Event::default().data(payload))),
        );
        Sse::new(sse_stream)
            .keep_alive(KeepAlive::default())
            .into_response()
    } else {
        let response = build_chat_completion_json(
            &req_id,
            created,
            &req.model,
            &exec.outcome.reply,
            prompt_tokens,
            completion_tokens,
        );
        Json(response).into_response()
    };

    attach_observability_headers(
        &mut resp,
        &exec.session.id,
        exec.mode.as_str(),
        exec.outcome.detected_model.as_deref(),
        exec.outcome.switched_model.as_deref(),
        exec.rotated_account_id.as_deref(),
    );
    resp
}

pub async fn responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ResponsesRequest>,
) -> Response {
    let Some(auth_key) = state.config.authenticate_headers(&headers) else {
        return openai_error(
            StatusCode::UNAUTHORIZED,
            "Invalid or missing API key",
            "invalid_request_error",
            "invalid_api_key",
        );
    };

    let messages = responses_to_chat_messages(req.instructions.as_deref(), &req.input);
    if messages.is_empty() {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "input must not be empty",
            "invalid_request_error",
            "invalid_input",
        );
    }

    let explicit_conv_id = headers
        .get("x-conversation-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let model_spec = state.config.find_model(&req.model);
    let acquired = match state.terminal_manager.acquire_session_for_turn(
        &auth_key,
        &req.model,
        model_spec.as_ref(),
        &messages,
        req.previous_response_id.as_deref(),
        explicit_conv_id.as_deref(),
        state.config.default_session_id.as_deref(),
    ) {
        Ok(acq) => acq,
        Err(err) => return map_acquire_error(err),
    };

    let req_id = generate_id("resp_");
    let timeout = Duration::from_secs(
        req.timeout_secs
            .unwrap_or(state.config.request_timeout_secs)
            .clamp(5, 3600),
    );
    let idle_quiesce = Duration::from_millis(state.config.idle_quiesce_ms);

    info!(
        request_id = %req_id,
        key_id = %auth_key.id,
        session_id = %acquired.session.id,
        model = %req.model,
        turn_mode = acquired.mode.as_str(),
        stream = req.stream,
        endpoint = "responses",
        "dispatching Responses API request to tmux terminal agent"
    );

    let exec = match run_turn_with_model_and_rotation(
        &state,
        &auth_key,
        model_spec.as_ref(),
        &messages,
        acquired.session,
        acquired.mode,
        acquired.messages_to_send,
        explicit_conv_id.as_deref(),
        timeout,
        idle_quiesce,
        &req_id,
    )
    .await
    {
        Ok(res) => res,
        Err(err_resp) => return err_resp,
    };

    let mut full_after_turn = messages.clone();
    full_after_turn.push(ChatMessage {
        role: "assistant".to_string(),
        content: MessageContent::Text(exec.outcome.reply.clone()),
    });
    let new_hash = canonical_messages_hash(&full_after_turn);
    exec.session
        .record_turn(new_hash, Some(req_id.clone()), explicit_conv_id);
    let _ = state.terminal_manager.persist_sessions();

    let created = now_millis() / 1000;
    let prompt_tokens = estimate_prompt_tokens(&messages);
    let completion_tokens = estimate_tokens(&exec.outcome.reply);

    let mut resp = if req.stream {
        let events = build_responses_sse_events(
            &req_id,
            created,
            &req.model,
            &exec.outcome.reply,
            prompt_tokens,
            completion_tokens,
        );
        let sse_stream = stream::iter(events.into_iter().map(|(evt_name, payload)| {
            let event = match evt_name {
                Some(name) => Event::default().event(name).data(payload),
                None => Event::default().data(payload),
            };
            Ok::<Event, Infallible>(event)
        }));
        Sse::new(sse_stream)
            .keep_alive(KeepAlive::default())
            .into_response()
    } else {
        let response = build_responses_json(
            &req_id,
            created,
            &req.model,
            &exec.outcome.reply,
            prompt_tokens,
            completion_tokens,
        );
        Json(response).into_response()
    };

    attach_observability_headers(
        &mut resp,
        &exec.session.id,
        exec.mode.as_str(),
        exec.outcome.detected_model.as_deref(),
        exec.outcome.switched_model.as_deref(),
        exec.rotated_account_id.as_deref(),
    );
    resp
}

fn attach_observability_headers(
    resp: &mut Response,
    session_id: &str,
    turn_mode: &str,
    detected_model: Option<&str>,
    switched_model: Option<&str>,
    rotated_account_id: Option<&str>,
) {
    if let Ok(val) = HeaderValue::from_str(session_id) {
        resp.headers_mut()
            .insert(HeaderName::from_static("x-gemini-proxy-session-id"), val);
    }
    if let Ok(val) = HeaderValue::from_str(turn_mode) {
        resp.headers_mut()
            .insert(HeaderName::from_static("x-gemini-proxy-turn-mode"), val);
    }
    if let Some(m) = detected_model {
        if let Ok(val) = HeaderValue::from_str(m) {
            resp.headers_mut()
                .insert(HeaderName::from_static("x-gemini-proxy-detected-model"), val);
        }
    }
    if let Some(m) = switched_model {
        if let Ok(val) = HeaderValue::from_str(m) {
            resp.headers_mut()
                .insert(HeaderName::from_static("x-gemini-proxy-model-switched"), val);
        }
    }
    if let Some(acc) = rotated_account_id {
        if let Ok(val) = HeaderValue::from_str(acc) {
            resp.headers_mut()
                .insert(HeaderName::from_static("x-gemini-proxy-account-rotated"), val);
        }
    }
}

pub fn responses_to_chat_messages(instructions: Option<&str>, input: &Value) -> Vec<ChatMessage> {
    let mut messages = Vec::new();
    if let Some(inst) = instructions.map(str::trim).filter(|s| !s.is_empty()) {
        messages.push(ChatMessage {
            role: "system".to_string(),
            content: MessageContent::Text(inst.to_string()),
        });
    }

    match input {
        Value::String(s) => {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                messages.push(ChatMessage {
                    role: "user".to_string(),
                    content: MessageContent::Text(trimmed.to_string()),
                });
            }
        }
        Value::Array(items) => {
            for item in items {
                if let Some(s) = item.as_str() {
                    let trimmed = s.trim();
                    if !trimmed.is_empty() {
                        messages.push(ChatMessage {
                            role: "user".to_string(),
                            content: MessageContent::Text(trimmed.to_string()),
                        });
                    }
                    continue;
                }
                let role = item
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or("user")
                    .to_string();
                let text = extract_responses_item_text(item);
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    messages.push(ChatMessage {
                        role,
                        content: MessageContent::Text(trimmed.to_string()),
                    });
                }
            }
        }
        Value::Object(_) => {
            let text = extract_responses_item_text(input);
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                let role = input
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or("user")
                    .to_string();
                messages.push(ChatMessage {
                    role,
                    content: MessageContent::Text(trimmed.to_string()),
                });
            }
        }
        _ => {}
    }

    messages
}

fn extract_responses_item_text(item: &Value) -> String {
    if let Some(s) = item.get("text").and_then(Value::as_str) {
        return s.to_string();
    }
    match item.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| part.as_str())
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

pub fn build_chat_completion_json(
    id: &str,
    created: u64,
    model: &str,
    content: &str,
    prompt_tokens: usize,
    completion_tokens: usize,
) -> Value {
    serde_json::json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [
            {
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": content,
                },
                "finish_reason": "stop",
            }
        ],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens,
        }
    })
}

pub fn build_sse_chunks(id: &str, created: u64, model: &str, content: &str) -> Vec<String> {
    let mut payloads = Vec::new();

    payloads.push(
        serde_json::json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{
                "index": 0,
                "delta": { "role": "assistant" },
                "finish_reason": null,
            }]
        })
        .to_string(),
    );

    if !content.is_empty() {
        payloads.push(
            serde_json::json!({
                "id": id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": { "content": content },
                    "finish_reason": null,
                }]
            })
            .to_string(),
        );
    }

    payloads.push(
        serde_json::json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "stop",
            }]
        })
        .to_string(),
    );
    payloads.push("[DONE]".to_string());
    payloads
}

pub fn build_responses_json(
    id: &str,
    created: u64,
    model: &str,
    content: &str,
    input_tokens: usize,
    output_tokens: usize,
) -> Value {
    serde_json::json!({
        "id": id,
        "object": "response",
        "created_at": created,
        "status": "completed",
        "model": model,
        "output": [
            {
                "id": format!("msg_{}", id.trim_start_matches("resp_")),
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [
                    {
                        "type": "output_text",
                        "text": content,
                        "annotations": [],
                    }
                ]
            }
        ],
        "output_text": content,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "total_tokens": input_tokens + output_tokens,
        }
    })
}

pub fn build_responses_sse_events(
    id: &str,
    created: u64,
    model: &str,
    content: &str,
    input_tokens: usize,
    output_tokens: usize,
) -> Vec<(Option<&'static str>, String)> {
    let mut events = Vec::new();

    events.push((
        Some("response.created"),
        serde_json::json!({
            "type": "response.created",
            "response": {
                "id": id,
                "object": "response",
                "created_at": created,
                "status": "in_progress",
                "model": model,
                "output": [],
            }
        })
        .to_string(),
    ));

    if !content.is_empty() {
        events.push((
            Some("response.output_text.delta"),
            serde_json::json!({
                "type": "response.output_text.delta",
                "output_index": 0,
                "content_index": 0,
                "delta": content,
            })
            .to_string(),
        ));
    }

    events.push((
        Some("response.output_text.done"),
        serde_json::json!({
            "type": "response.output_text.done",
            "output_index": 0,
            "content_index": 0,
            "text": content,
        })
        .to_string(),
    ));

    events.push((
        Some("response.completed"),
        serde_json::json!({
            "type": "response.completed",
            "response": build_responses_json(id, created, model, content, input_tokens, output_tokens),
        })
        .to_string(),
    ));

    events.push((None, "[DONE]".to_string()));
    events
}

fn generate_id(prefix: &str) -> String {
    let mut bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{prefix}{hex}")
}

fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4).max(1)
}

fn estimate_prompt_tokens(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .map(|m| estimate_tokens(&m.content.as_plain_text()))
        .sum()
}

#[allow(dead_code)]
fn _unused_path_marker(_p: Path<String>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_valid_openai_chat_completion_response() {
        let json = build_chat_completion_json("chatcmpl-1", 1000, "agy-1", "hello", 4, 2);
        assert_eq!(json["object"], "chat.completion");
        assert_eq!(json["model"], "agy-1");
        assert_eq!(json["choices"][0]["message"]["role"], "assistant");
        assert_eq!(json["choices"][0]["message"]["content"], "hello");
        assert_eq!(json["choices"][0]["finish_reason"], "stop");
        assert_eq!(json["usage"]["total_tokens"], 6);
    }

    #[test]
    fn builds_valid_sse_chunks_ending_with_done() {
        let chunks = build_sse_chunks("chatcmpl-2", 1000, "agy-1", "hi");
        assert_eq!(chunks.len(), 4);
        let first: Value = serde_json::from_str(&chunks[0]).unwrap();
        assert_eq!(first["object"], "chat.completion.chunk");
        assert_eq!(first["choices"][0]["delta"]["role"], "assistant");
        let second: Value = serde_json::from_str(&chunks[1]).unwrap();
        assert_eq!(second["choices"][0]["delta"]["content"], "hi");
        let third: Value = serde_json::from_str(&chunks[2]).unwrap();
        assert_eq!(third["choices"][0]["finish_reason"], "stop");
        assert_eq!(chunks[3], "[DONE]");
    }

    #[test]
    fn parses_responses_string_and_array_inputs() {
        let msgs = responses_to_chat_messages(Some("be brief"), &Value::String("ping".into()));
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[1].role, "user");
        assert_eq!(msgs[1].content.as_plain_text(), "ping");

        let array_input = serde_json::json!([
            {
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": "hello responses" }]
            }
        ]);
        let msgs2 = responses_to_chat_messages(None, &array_input);
        assert_eq!(msgs2.len(), 1);
        assert_eq!(msgs2[0].content.as_plain_text(), "hello responses");
    }

    #[test]
    fn builds_valid_responses_json_and_sse() {
        let json = build_responses_json("resp_1", 1000, "gemini-proxy-agent", "pong", 2, 1);
        assert_eq!(json["object"], "response");
        assert_eq!(json["status"], "completed");
        assert_eq!(json["output_text"], "pong");
        assert_eq!(json["output"][0]["content"][0]["text"], "pong");

        let events = build_responses_sse_events("resp_2", 1000, "gemini-proxy-agent", "pong", 2, 1);
        assert_eq!(events.len(), 5);
        assert_eq!(events[0].0, Some("response.created"));
        assert_eq!(events[1].0, Some("response.output_text.delta"));
        assert_eq!(events[3].0, Some("response.completed"));
        assert_eq!(events[4].1, "[DONE]");
    }
}
