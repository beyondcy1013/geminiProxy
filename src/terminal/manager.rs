use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex, RwLock},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use rand::RngCore;
use serde::Serialize;
use tracing::{info, warn};

use crate::config::{AuthenticatedKey, ModelSpec};

use super::{
    agent::{
        ChatMessage, DetectedAgent, canonical_messages_hash, detect_active_model_from_pane,
        detect_running_agent_for_session, pane_fingerprint, pane_has_working_indicator,
    },
    session::{
        AgentKind, ConversationState, StoredSessionMeta, TerminalSession, capture_session_plain,
        now_millis,
    },
    tmux::{destroy_tmux_session, ensure_tmux_session, tmux_session_exists},
};

#[derive(Debug, Clone, Serialize)]
pub struct SessionView {
    pub id: String,
    pub name: String,
    pub model_alias: String,
    pub cwd: PathBuf,
    pub startup_command: Option<String>,
    pub agent_kind: AgentKind,
    pub detected_agent: String,
    pub detected_model: Option<String>,
    pub use_markers: bool,
    pub owner_key_id: Option<String>,
    pub managed: bool,
    pub turn_count: usize,
    pub alive: bool,
    pub busy: bool,
    pub created_at: u64,
    pub last_used_at: u64,
}

#[derive(Debug, Clone)]
pub struct CreateSessionParams {
    pub name: String,
    pub model_alias: Option<String>,
    pub cwd: Option<PathBuf>,
    pub startup_command: Option<String>,
    pub agent_kind: AgentKind,
    pub use_markers: bool,
    pub owner_key_id: Option<String>,
    pub managed: bool,
    pub explicit_conversation_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnMode {
    New,
    Continue,
}

impl TurnMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Continue => "continue",
        }
    }
}

pub struct AcquiredSession {
    pub session: Arc<TerminalSession>,
    pub mode: TurnMode,
    pub messages_to_send: Vec<ChatMessage>,
}

#[derive(Debug)]
pub enum AcquireError {
    Forbidden(String),
    ModelNotFound(String),
    PreviousResponseNotFound(String),
    TooManyBusySessions(String),
    Internal(anyhow::Error),
}

pub struct TerminalManager {
    sessions_file: PathBuf,
    sessions: RwLock<HashMap<String, Arc<TerminalSession>>>,
    launch_lock: StdMutex<()>,
}

impl TerminalManager {
    pub fn new(data_dir: &Path) -> Result<Arc<Self>> {
        fs::create_dir_all(data_dir)?;
        let sessions_file = data_dir.join("sessions.json");
        let manager = Arc::new(Self {
            sessions_file,
            sessions: RwLock::new(HashMap::new()),
            launch_lock: StdMutex::new(()),
        });
        manager.restore_persisted_sessions();
        Ok(manager)
    }

    fn restore_persisted_sessions(&self) {
        if cfg!(windows) {
            // Windows ConPTY child processes do not outlive the proxy process; clear stale entries.
            let _ = self.persist_sessions();
            return;
        }
        if !self.sessions_file.is_file() {
            return;
        }
        let Ok(raw) = fs::read_to_string(&self.sessions_file) else {
            return;
        };
        let Ok(stored) = serde_json::from_str::<Vec<StoredSessionMeta>>(&raw) else {
            warn!("failed to parse {}", self.sessions_file.display());
            return;
        };

        let mut restored = HashMap::new();
        for meta in stored {
            if !tmux_session_exists(&meta.id) {
                warn!(session_id = %meta.id, "tmux session no longer exists on disk, skipping restore");
                continue;
            }
            match TerminalSession::attach(meta.clone()) {
                Ok(session) => {
                    info!(session_id = %session.id, model = %session.model_alias, "restored tmux session");
                    restored.insert(session.id.clone(), session);
                }
                Err(err) => {
                    warn!(session_id = %meta.id, "failed to re-attach tmux session: {err:#}");
                }
            }
        }

        if let Ok(mut guard) = self.sessions.write() {
            *guard = restored;
        }
        let _ = self.persist_sessions();
    }

    pub fn persist_sessions(&self) -> Result<()> {
        let metas: Vec<StoredSessionMeta> = self
            .sessions
            .read()
            .map(|guard| {
                let mut list: Vec<StoredSessionMeta> =
                    guard.values().map(|s| s.to_meta()).collect();
                list.sort_by_key(|m| m.created_at);
                list
            })
            .unwrap_or_default();
        let json = serde_json::to_string_pretty(&metas)?;
        fs::write(&self.sessions_file, json)
            .with_context(|| format!("failed to write {}", self.sessions_file.display()))?;
        Ok(())
    }

    pub fn create_session(&self, params: CreateSessionParams) -> Result<Arc<TerminalSession>> {
        let id = generate_session_id();
        let name = params.name.trim();
        anyhow::ensure!(!name.is_empty(), "session name must not be empty");

        let model_alias = params
            .model_alias
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(name)
            .to_string();

        let cwd = params
            .cwd
            .filter(|p| p.is_dir())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));

        let startup_command = params
            .startup_command
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let now = now_millis();
        let meta = StoredSessionMeta {
            id: id.clone(),
            name: name.to_string(),
            model_alias,
            cwd: cwd.clone(),
            startup_command: startup_command.clone(),
            agent_kind: params.agent_kind,
            use_markers: params.use_markers,
            owner_key_id: params.owner_key_id,
            managed: params.managed,
            conversation_state: ConversationState {
                explicit_conversation_id: params.explicit_conversation_id,
                ..ConversationState::default()
            },
            created_at: now,
            last_used_at: now,
        };

        ensure_tmux_session(&id, &cwd, startup_command.as_deref())?;
        let session = match TerminalSession::attach(meta) {
            Ok(session) => session,
            Err(err) => {
                let _ = destroy_tmux_session(&id);
                return Err(err);
            }
        };

        if let Ok(mut guard) = self.sessions.write() {
            guard.insert(id, session.clone());
        }
        self.persist_sessions()?;
        Ok(session)
    }

    pub fn get_session(&self, id: &str) -> Option<Arc<TerminalSession>> {
        self.sessions.read().ok()?.get(id).cloned()
    }

    pub fn list_sessions(&self) -> Vec<Arc<TerminalSession>> {
        let mut list: Vec<Arc<TerminalSession>> = self
            .sessions
            .read()
            .map(|guard| guard.values().cloned().collect())
            .unwrap_or_default();
        list.sort_by_key(|s| s.created_at);
        list
    }

    pub fn list_session_views(&self) -> Vec<SessionView> {
        self.list_sessions()
            .into_iter()
            .map(|s| {
                let detected: DetectedAgent = detect_running_agent_for_session(&s);
                let detected_model = capture_session_plain(&s, 60)
                    .ok()
                    .and_then(|pane| detect_active_model_from_pane(&pane, detected));
                SessionView {
                    id: s.id.clone(),
                    name: s.name.clone(),
                    model_alias: s.model_alias.clone(),
                    cwd: s.cwd.clone(),
                    startup_command: s.startup_command.clone(),
                    agent_kind: s.agent_kind,
                    detected_agent: detected.label().to_string(),
                    detected_model,
                    use_markers: s.use_markers,
                    owner_key_id: s.owner_key_id.clone(),
                    managed: s.managed,
                    turn_count: s.turn_count(),
                    alive: s.is_alive() && tmux_session_exists(&s.id),
                    busy: s.is_busy(),
                    created_at: s.created_at,
                    last_used_at: s.last_used_at_ms(),
                }
            })
            .collect()
    }

    pub fn delete_session(&self, id: &str) -> Result<bool> {
        let removed = self
            .sessions
            .write()
            .ok()
            .and_then(|mut guard| guard.remove(id));
        let Some(session) = removed else {
            return Ok(false);
        };
        session.detach_client();
        destroy_tmux_session(id)?;
        self.persist_sessions()?;
        Ok(true)
    }

    pub fn acquire_session_for_turn(
        &self,
        auth_key: &AuthenticatedKey,
        requested_model: &str,
        model_spec: Option<&ModelSpec>,
        messages: &[ChatMessage],
        previous_response_id: Option<&str>,
        explicit_conv_id: Option<&str>,
        default_session_id: Option<&str>,
    ) -> std::result::Result<AcquiredSession, AcquireError> {
        let target_model = resolve_target_model_name(
            requested_model,
            model_spec,
            &self.list_sessions(),
            default_session_id,
        )
        .ok_or_else(|| {
            AcquireError::ModelNotFound(format!(
                "no configured model or manual session found for '{requested_model}'"
            ))
        })?;

        if !auth_key.can_access_model(&target_model) {
            return Err(AcquireError::Forbidden(format!(
                "API key '{}' is not permitted to access model '{}'",
                auth_key.name, target_model
            )));
        }

        // Case A: Explicit continuation via OpenAI Responses `previous_response_id`.
        if let Some(prev_id) = previous_response_id.map(str::trim).filter(|s| !s.is_empty()) {
            let sessions = self.list_sessions();
            if let Some(session) = sessions.iter().find(|s| s.has_response_id(prev_id)) {
                if session.owner_key_id.as_deref() != Some(auth_key.id.as_str()) {
                    return Err(AcquireError::Forbidden(
                        "previous_response_id belongs to a different API key".to_string(),
                    ));
                }
                if session.model_alias != target_model {
                    return Err(AcquireError::Forbidden(format!(
                        "previous_response_id belongs to model '{}', not '{}'",
                        session.model_alias, target_model
                    )));
                }
                if !session.is_alive() || !tmux_session_exists(&session.id) {
                    return Err(AcquireError::PreviousResponseNotFound(format!(
                        "terminal session for previous_response_id '{prev_id}' is no longer alive"
                    )));
                }
                session.touch_last_used();
                let delta = extract_delta_messages(messages);
                return Ok(AcquiredSession {
                    session: session.clone(),
                    mode: TurnMode::Continue,
                    messages_to_send: delta,
                });
            }
            return Err(AcquireError::PreviousResponseNotFound(format!(
                "previous_response_id '{prev_id}' not found"
            )));
        }

        // Case B: Prefix-hash verified continuation (only when client passes prior assistant history).
        let sessions = self.list_sessions();
        if let Some((prefix, delta)) = split_continuation_prefix(messages) {
            let prefix_hash = canonical_messages_hash(prefix);
            if let Some(existing) = find_matching_continuation_session(
                &sessions,
                &auth_key.id,
                &target_model,
                &prefix_hash,
                explicit_conv_id,
            ) {
                if existing.is_alive() && tmux_session_exists(&existing.id) {
                    existing.touch_last_used();
                    return Ok(AcquiredSession {
                        session: existing,
                        mode: TurnMode::Continue,
                        messages_to_send: delta.to_vec(),
                    });
                }
            }
        }

        // Case C: New conversation (or edited/forked history without exact match) ->
        // must isolate in a clean session so context never leaks across conversations/keys.
        if let Some(spec) = model_spec {
            let session = self.spawn_managed_session(auth_key, spec, explicit_conv_id)?;
            return Ok(AcquiredSession {
                session,
                mode: TurnMode::New,
                messages_to_send: messages.to_vec(),
            });
        }

        // Fallback for manually created Web UI sessions (no ModelSpec configured):
        // Only allow an unmanaged session if it has zero prior turns (clean), or matches
        // owner_key_id == None / auth_key.id. Never mix an unrelated conversation's dirty session.
        if let Some(manual) = sessions.iter().find(|s| {
            !s.managed
                && s.model_alias == target_model
                && (s.owner_key_id.is_none()
                    || s.owner_key_id.as_deref() == Some(auth_key.id.as_str()))
                && s.turn_count() == 0
                && s.is_alive()
                && tmux_session_exists(&s.id)
        }) {
            manual.touch_last_used();
            return Ok(AcquiredSession {
                session: manual.clone(),
                mode: TurnMode::New,
                messages_to_send: messages.to_vec(),
            });
        }

        Err(AcquireError::ModelNotFound(format!(
            "no fresh session or ModelSpec launch_command available for '{target_model}'"
        )))
    }

    fn spawn_managed_session(
        &self,
        auth_key: &AuthenticatedKey,
        spec: &ModelSpec,
        explicit_conv_id: Option<&str>,
    ) -> std::result::Result<Arc<TerminalSession>, AcquireError> {
        let _lock = self
            .launch_lock
            .lock()
            .map_err(|_| AcquireError::Internal(anyhow::anyhow!("launch lock poisoned")))?;

        let mut managed_for_model: Vec<Arc<TerminalSession>> = self
            .list_sessions()
            .into_iter()
            .filter(|s| s.managed && s.model_alias == spec.id)
            .collect();
        managed_for_model.sort_by_key(|s| s.last_used_at_ms());

        // Purge any dead managed sessions first.
        for dead in managed_for_model
            .iter()
            .filter(|s| !s.is_alive() || !tmux_session_exists(&s.id))
        {
            let _ = self.delete_session(&dead.id);
        }
        managed_for_model.retain(|s| s.is_alive() && tmux_session_exists(&s.id));

        if managed_for_model.len() >= spec.max_sessions {
            let Some(victim) = managed_for_model.iter().find(|s| !s.is_busy()) else {
                return Err(AcquireError::TooManyBusySessions(format!(
                    "model '{}' reached max_sessions ({}) and all sessions are busy",
                    spec.id, spec.max_sessions
                )));
            };
            info!(
                model = %spec.id,
                evicted_session_id = %victim.id,
                "evicting oldest idle managed tmux session to isolate new conversation"
            );
            self.delete_session(&victim.id)
                .map_err(AcquireError::Internal)?;
        }

        let short_tag: String = generate_session_id();
        let target_upstream = spec.upstream_model.as_deref().unwrap_or(&spec.id);
        let launch_command = spec.launch_command.replace("{model}", target_upstream);
        let session = self
            .create_session(CreateSessionParams {
                name: format!("{}-{}", spec.id, short_tag),
                model_alias: Some(spec.id.clone()),
                cwd: Some(spec.cwd.clone()),
                startup_command: Some(launch_command.clone()),
                agent_kind: spec.agent_kind,
                use_markers: spec.use_markers,
                owner_key_id: Some(auth_key.id.clone()),
                managed: true,
                explicit_conversation_id: explicit_conv_id.map(str::to_string),
            })
            .map_err(AcquireError::Internal)?;

        wait_for_agent_startup(
            &session,
            spec.agent_kind,
            &launch_command,
            Duration::from_millis(spec.startup_settle_ms),
        );
        info!(
            session_id = %session.id,
            model = %spec.id,
            owner_key_id = %auth_key.id,
            "auto-launched isolated tmux agent session"
        );
        Ok(session)
    }

    pub fn replace_exhausted_session(
        &self,
        exhausted_id: &str,
        auth_key: &AuthenticatedKey,
        spec: &ModelSpec,
        explicit_conv_id: Option<&str>,
    ) -> std::result::Result<Arc<TerminalSession>, AcquireError> {
        let _ = self.delete_session(exhausted_id);
        self.spawn_managed_session(auth_key, spec, explicit_conv_id)
    }

    pub fn reap_expired_managed_sessions(&self, models: &[ModelSpec]) {
        let ttl_by_model: HashMap<&str, u64> = models
            .iter()
            .map(|m| (m.id.as_str(), m.idle_ttl_secs * 1000))
            .collect();
        let now = now_millis();

        for session in self.list_sessions() {
            if !session.managed || session.is_busy() {
                continue;
            }
            let ttl_ms = ttl_by_model
                .get(session.model_alias.as_str())
                .copied()
                .unwrap_or(1800 * 1000);
            if now.saturating_sub(session.last_used_at_ms()) >= ttl_ms {
                info!(
                    session_id = %session.id,
                    model = %session.model_alias,
                    "reaping expired idle managed tmux session"
                );
                let _ = self.delete_session(&session.id);
            }
        }
    }
}

fn wait_for_agent_startup(
    session: &TerminalSession,
    agent_kind: AgentKind,
    launch_command: &str,
    min_settle: Duration,
) {
    if !min_settle.is_zero() {
        std::thread::sleep(min_settle);
    }
    let lower_cmd = launch_command.to_ascii_lowercase();
    let expects_tui = matches!(
        agent_kind,
        AgentKind::CodeBuddy | AgentKind::Agy | AgentKind::Codex | AgentKind::Claude
    ) || ["codebuddy", "cbc", "agy", "codex", "claude"]
        .iter()
        .any(|kw| lower_cmd.contains(kw));
    let deadline = Instant::now() + Duration::from_secs(if expects_tui { 12 } else { 5 });
    let mut last_fp = 0u64;
    let mut stable_ticks = 0u8;
    let cmd_first_token = launch_command.split_whitespace().next().unwrap_or("");
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
        let Ok(pane) = capture_session_plain(session, 100) else {
            break;
        };
        let non_empty_lines: Vec<&str> = pane
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        let saw_output_after_cmd = if !cmd_first_token.is_empty() {
            if let Some(cmd_idx) = non_empty_lines
                .iter()
                .position(|l| l.contains(cmd_first_token))
            {
                cmd_idx + 1 < non_empty_lines.len()
            } else {
                !non_empty_lines.is_empty()
            }
        } else {
            !non_empty_lines.is_empty()
        };
        let tui_ready = !expects_tui
            || pane.contains("← for agents")
            || pane.contains("? for shortcuts")
            || pane.contains("Context ")
            || detect_active_model_from_pane(&pane, DetectedAgent::Generic).is_some();
        let fp = pane_fingerprint(&pane);
        let working = pane_has_working_indicator(&pane);
        if saw_output_after_cmd && tui_ready && fp == last_fp && !working {
            stable_ticks += 1;
            if stable_ticks >= 2 {
                break;
            }
        } else {
            last_fp = fp;
            stable_ticks = 0;
        }
    }
}

fn resolve_target_model_name(
    requested_model: &str,
    model_spec: Option<&ModelSpec>,
    sessions: &[Arc<TerminalSession>],
    default_session_id: Option<&str>,
) -> Option<String> {
    if let Some(spec) = model_spec {
        return Some(spec.id.clone());
    }
    let target = requested_model.trim();
    if let Some(s) = sessions
        .iter()
        .find(|s| s.model_alias == target || s.name == target || s.id == target)
    {
        return Some(s.model_alias.clone());
    }
    if target.eq_ignore_ascii_case("default") || target.is_empty() {
        if let Some(default_id) = default_session_id {
            if let Some(s) = sessions.iter().find(|s| s.id == default_id) {
                return Some(s.model_alias.clone());
            }
        }
        if sessions.len() == 1 {
            return Some(sessions[0].model_alias.clone());
        }
    }
    None
}

pub fn split_continuation_prefix(messages: &[ChatMessage]) -> Option<(&[ChatMessage], &[ChatMessage])> {
    if messages.len() < 2 {
        return None;
    }
    let last = messages.last()?;
    if !last.role.eq_ignore_ascii_case("user") {
        return None;
    }
    let prefix = &messages[..messages.len() - 1];
    if !prefix
        .iter()
        .any(|m| m.role.eq_ignore_ascii_case("assistant"))
    {
        return None;
    }
    Some((prefix, &messages[messages.len() - 1..]))
}

fn extract_delta_messages(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    if let Some(last) = messages.last() {
        vec![last.clone()]
    } else {
        messages.to_vec()
    }
}

fn find_matching_continuation_session(
    sessions: &[Arc<TerminalSession>],
    owner_key_id: &str,
    model_id: &str,
    prefix_hash: &str,
    explicit_conv_id: Option<&str>,
) -> Option<Arc<TerminalSession>> {
    sessions
        .iter()
        .filter(|s| {
            s.owner_key_id.as_deref() == Some(owner_key_id)
                && s.model_alias == model_id
                && s.last_history_hash().as_deref() == Some(prefix_hash)
                && match explicit_conv_id {
                    Some(cid) => s.explicit_conversation_id().as_deref() == Some(cid),
                    None => true,
                }
        })
        .max_by_key(|s| s.last_used_at_ms())
        .cloned()
}

fn generate_session_id() -> String {
    let mut bytes = [0u8; 4];
    rand::thread_rng().fill_bytes(&mut bytes);
    format!(
        "s{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::agent::MessageContent;

    fn msg(role: &str, text: &str) -> ChatMessage {
        ChatMessage {
            role: role.to_string(),
            content: MessageContent::Text(text.to_string()),
        }
    }

    #[test]
    fn continuation_prefix_requires_prior_assistant_turn_and_trailing_user() {
        assert!(split_continuation_prefix(&[msg("user", "hi")]).is_none());
        assert!(split_continuation_prefix(&[msg("system", "s"), msg("user", "hi")]).is_none());

        let conv = vec![
            msg("user", "my secret is 42"),
            msg("assistant", "noted"),
            msg("user", "what is my secret?"),
        ];
        let (prefix, delta) = split_continuation_prefix(&conv).unwrap();
        assert_eq!(prefix.len(), 2);
        assert_eq!(delta.len(), 1);
        assert_eq!(delta[0].content.as_plain_text(), "what is my secret?");
    }

    #[test]
    fn canonical_hash_detects_edited_history_and_role_changes() {
        let h1 = canonical_messages_hash(&[msg("user", "a"), msg("assistant", "b")]);
        let h2 = canonical_messages_hash(&[msg("user", "a"), msg("assistant", "b_edited")]);
        let h3 = canonical_messages_hash(&[msg("USER", " a "), msg("assistant", "b")]);
        assert_ne!(h1, h2);
        assert_eq!(h1, h3);
    }
}
