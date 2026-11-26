use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
#[cfg(target_os = "linux")]
use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{Context, Result};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{
    session::{AgentKind, TerminalSession, capture_session_plain, send_session_paste},
    tmux::tmux_pane_pid,
};

pub fn canonical_messages_hash(messages: &[ChatMessage]) -> String {
    let mut hasher = Sha256::new();
    hasher.update((messages.len() as u64).to_be_bytes());
    for msg in messages {
        let role = msg.role.trim().to_ascii_lowercase();
        let text = msg.content.as_plain_text();
        let trimmed = text.trim();
        hasher.update((role.len() as u64).to_be_bytes());
        hasher.update(role.as_bytes());
        hasher.update((trimmed.len() as u64).to_be_bytes());
        hasher.update(trimmed.as_bytes());
    }
    hex::encode(hasher.finalize())
}

const PANE_CAPTURE_LINES: u32 = 500;
const POLL_INTERVAL: Duration = Duration::from_millis(200);

const QUOTA_EXHAUSTED_MARKERS: &[&str] = &[
    "余额不足",
    "积分不足",
    "额度不足",
    "额度已用尽",
    "配额超限",
    "超出使用限制",
    "使用量已超出",
    "您的使用量已超出频率限制",
    "insufficient balance",
    "insufficient credits",
    "credit balance is too low",
    "insufficient quota",
    "quota exceeded",
    "usage limit reached",
    "request limit reached",
    "rate limit reached",
    "rate limit exceeded",
    "individual quota reached",
    "you have exhausted your quota on this model",
    "upgrade your subscription to increase your limits",
    "authentication expired. please log back in.",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default)]
    pub content: MessageContent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl Default for MessageContent {
    fn default() -> Self {
        Self::Text(String::new())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentPart {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub text: Option<String>,
}

impl MessageContent {
    pub fn as_plain_text(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Parts(parts) => parts
                .iter()
                .filter_map(|p| p.text.as_deref())
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectedAgent {
    Agy,
    Codex,
    Claude,
    CodeBuddy,
    Generic,
}

impl DetectedAgent {
    pub fn label(self) -> &'static str {
        match self {
            Self::Agy => "agy",
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::CodeBuddy => "codebuddy",
            Self::Generic => "shell/generic",
        }
    }

    pub fn supports_slash_model(self) -> bool {
        matches!(
            self,
            Self::Agy | Self::Codex | Self::Claude | Self::CodeBuddy
        )
    }
}

pub fn detect_running_agent_for_session(session: &TerminalSession) -> DetectedAgent {
    let detected = detect_running_agent(&session.id, session.agent_kind);
    if detected != DetectedAgent::Generic {
        return detected;
    }
    if let Some(cmd) = session.startup_command.as_deref() {
        let lower = cmd.to_ascii_lowercase();
        if lower.contains("codebuddy") || lower.starts_with("cbc") {
            return DetectedAgent::CodeBuddy;
        }
        if lower.contains("agy") || lower.contains("antigravity") {
            return DetectedAgent::Agy;
        }
        if lower.contains("codex") {
            return DetectedAgent::Codex;
        }
        if lower.contains("claude") {
            return DetectedAgent::Claude;
        }
    }
    DetectedAgent::Generic
}

pub fn detect_running_agent(session_id: &str, configured: AgentKind) -> DetectedAgent {
    match configured {
        AgentKind::Agy => return DetectedAgent::Agy,
        AgentKind::Codex => return DetectedAgent::Codex,
        AgentKind::Claude => return DetectedAgent::Claude,
        AgentKind::CodeBuddy => return DetectedAgent::CodeBuddy,
        AgentKind::Generic => return DetectedAgent::Generic,
        AgentKind::Auto => {}
    }

    #[cfg(target_os = "linux")]
    {
        let Ok(Some(pane_pid)) = tmux_pane_pid(session_id) else {
            return DetectedAgent::Generic;
        };

        let descendants = read_descendant_pids(pane_pid);
        for pid in descendants {
            let comm = fs::read_to_string(format!("/proc/{pid}/comm"))
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase();
            let cmdline = fs::read(format!("/proc/{pid}/cmdline"))
                .map(|bytes| {
                    bytes
                        .split(|b| *b == 0)
                        .filter(|s| !s.is_empty())
                        .map(|s| String::from_utf8_lossy(s).to_ascii_lowercase())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();

            if comm == "codebuddy"
                || comm == "cbc"
                || cmdline.contains("codebuddy-code")
                || cmdline.contains("/codebuddy")
                || cmdline.starts_with("codebuddy ")
            {
                return DetectedAgent::CodeBuddy;
            }
            if comm == "agy" || comm == "antigravity" || cmdline.contains("antigravity-cli") {
                return DetectedAgent::Agy;
            }
            if comm == "codex" || cmdline.contains("/codex") || cmdline.starts_with("codex ") {
                return DetectedAgent::Codex;
            }
            if comm == "claude" || cmdline.contains("/claude") || cmdline.starts_with("claude ") {
                return DetectedAgent::Claude;
            }
        }
    }

    let _ = session_id;
    let _ = tmux_pane_pid;
    DetectedAgent::Generic
}

#[cfg(target_os = "linux")]
fn read_descendant_pids(root_pid: u32) -> Vec<u32> {
    let mut children_by_parent: HashMap<u32, Vec<u32>> = HashMap::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return vec![root_pid];
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        if let Some(ppid) = read_ppid(&entry.path().join("stat")) {
            children_by_parent.entry(ppid).or_default().push(pid);
        }
    }

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([root_pid]);
    while let Some(pid) = queue.pop_front() {
        if !seen.insert(pid) {
            continue;
        }
        out.push(pid);
        if let Some(children) = children_by_parent.get(&pid) {
            queue.extend(children.iter().copied());
        }
    }
    out
}

#[cfg(target_os = "linux")]
fn read_ppid(stat_path: &Path) -> Option<u32> {
    let stat = fs::read_to_string(stat_path).ok()?;
    let close_paren = stat.rfind(')')?;
    let rest = stat.get(close_paren + 1..)?.trim_start();
    let mut fields = rest.split_whitespace();
    let _state = fields.next()?;
    fields.next()?.parse().ok()
}

pub fn pane_has_quota_exhausted(text: &str) -> Option<&'static str> {
    let lower = text.to_ascii_lowercase();
    for marker in QUOTA_EXHAUSTED_MARKERS {
        if lower.contains(&marker.to_ascii_lowercase()) {
            return Some(*marker);
        }
    }
    None
}

pub fn detect_active_model_from_pane(pane: &str, agent: DetectedAgent) -> Option<String> {
    let lines: Vec<&str> = pane
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.is_empty() {
        return None;
    }

    let window = lines.iter().rev().take(25);
    match agent {
        DetectedAgent::CodeBuddy => {
            for &line in window.clone() {
                if let Some(m) = extract_codebuddy_status_line(line) {
                    return Some(m);
                }
            }
        }
        DetectedAgent::Agy => {
            for &line in window.clone() {
                if let Some(m) = extract_agy_prompt_line(line) {
                    return Some(m);
                }
            }
        }
        DetectedAgent::Codex => {
            for &line in window.clone() {
                if let Some(m) = extract_codex_status_line(line) {
                    return Some(m);
                }
            }
        }
        DetectedAgent::Claude => {
            for &line in window.clone() {
                if let Some(m) = extract_claude_banner_line(line) {
                    return Some(m);
                }
            }
        }
        DetectedAgent::Generic => {}
    }

    for &line in window {
        if let Some(m) = extract_codebuddy_status_line(line) {
            return Some(m);
        }
        if let Some(m) = extract_agy_prompt_line(line) {
            return Some(m);
        }
        if let Some(m) = extract_codex_status_line(line) {
            return Some(m);
        }
        if let Some(m) = extract_claude_banner_line(line) {
            return Some(m);
        }
    }
    None
}

fn is_valid_model_name_token(candidate: &str) -> bool {
    let trimmed = candidate.trim();
    if trimmed.len() < 2 || trimmed.len() > 48 || !trimmed.is_ascii() {
        return false;
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ' '))
    {
        return false;
    }
    let first = trimmed.chars().next().unwrap_or(' ');
    let last = trimmed.chars().last().unwrap_or(' ');
    if !first.is_ascii_alphanumeric() || !last.is_ascii_alphanumeric() {
        return false;
    }
    if !trimmed.chars().any(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    !matches!(
        lower.as_str(),
        "context"
            | "used"
            | "shortcuts"
            | "cancel"
            | "agent"
            | "agents"
            | "default"
            | "auto"
            | "low"
            | "medium"
            | "high"
            | "xhigh"
            | "bypass"
            | "permissions"
    )
}

fn extract_codebuddy_status_line(line: &str) -> Option<String> {
    if !line.contains("← for agents") {
        return None;
    }
    let (before, _) = line.split_once("← for agents")?;
    let first_seg = before.split('·').next().unwrap_or("").trim();
    let mut words: Vec<&str> = first_seg.split_whitespace().collect();
    if let Some(last) = words.last() {
        if matches!(
            last.to_ascii_lowercase().as_str(),
            "low" | "medium" | "high" | "xhigh" | "max"
        ) && words.len() > 1
        {
            words.pop();
        }
    }
    let candidate = words.join(" ");
    is_valid_model_name_token(&candidate).then_some(candidate)
}

fn extract_codex_status_line(line: &str) -> Option<String> {
    if !line.contains("Context") || !line.contains("% used") {
        return None;
    }
    let (before, _) = line
        .split_once("· Context")
        .or_else(|| line.split_once("Context"))?;
    let model_part = before.trim().split('·').next().unwrap_or(before).trim();
    let mut words: Vec<&str> = model_part.split_whitespace().collect();
    if let Some(last) = words.last() {
        if matches!(
            last.to_ascii_lowercase().as_str(),
            "low" | "medium" | "high" | "xhigh" | "max"
        ) && words.len() > 1
        {
            words.pop();
        }
    }
    let candidate = words.join(" ");
    is_valid_model_name_token(&candidate).then_some(candidate)
}

fn extract_agy_prompt_line(line: &str) -> Option<String> {
    if (!line.contains("? for shortcuts") && !line.contains("esc to cancel"))
        || line.contains("← for agents")
    {
        return None;
    }
    let (_, right) = line
        .split_once("? for shortcuts")
        .or_else(|| line.split_once("esc to cancel"))?;
    let right_lower = right.to_ascii_lowercase();
    if right_lower.contains("warning") || right_lower.contains("f2 to view") {
        return None;
    }
    for seg in right.split('·').map(str::trim) {
        if is_valid_model_name_token(seg) {
            return Some(seg.to_string());
        }
    }
    None
}

fn extract_claude_banner_line(line: &str) -> Option<String> {
    if line.starts_with("Model:") || line.contains(" Model:") {
        let (_, after) = line.split_once("Model:")?;
        let cand = after.split_whitespace().next().unwrap_or("").trim();
        if is_valid_model_name_token(cand) {
            return Some(cand.to_string());
        }
    }
    None
}

fn models_match(current: &str, desired: &str) -> bool {
    let norm = |s: &str| {
        s.trim()
            .to_ascii_lowercase()
            .replace(['_', ' '], "-")
    };
    let a = norm(current);
    let b = norm(desired);
    a == b || a.contains(&b) || b.contains(&a)
}

/// Checks the tail of a plain tmux pane capture for active TUI agent working indicators.
pub fn pane_has_working_indicator(pane: &str) -> bool {
    let lines: Vec<&str> = pane
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let start = lines.len().saturating_sub(14);
    for line in &lines[start..] {
        if line.starts_with(['>', '❯', '›', '$', '#']) {
            continue;
        }
        if let Some(first) = line.chars().next() {
            if ('\u{2800}'..='\u{28ff}').contains(&first) {
                return true;
            }
            if matches!(
                first,
                '✻' | '✢' | '✶' | '✷' | '✸' | '✽' | '✺' | '✹' | '∴'
            ) && (line.contains('…') || line.contains("..."))
            {
                return true;
            }
        }
        let lower = line.to_ascii_lowercase();
        if lower.contains("esc to interrupt")
            || lower.contains("esc to cancel")
            || lower.contains("press esc to interrupt")
            || lower.contains("working (")
            || lower.contains("thinking…")
            || lower.contains("thinking...")
            || lower.contains("acquiring optimized context")
        {
            return true;
        }
    }
    false
}

/// Computes a pane fingerprint that ignores volatile idle footer/particle lines.
pub fn pane_fingerprint(pane: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    let mut lines: Vec<&str> = pane.lines().collect();
    if let Some(footer) = lines.iter().rposition(|line| {
        line.contains(" · Context ") && line.contains(" used · ") && line.contains(" · Fast ")
    }) {
        lines[footer] = "";
    }
    for line in &lines {
        let clean: String = line
            .chars()
            .filter(|ch| !matches!(ch, '⠁' | '⠂' | '⠄' | '⠈' | '⠐' | '⠠' | '⡀' | '⢀'))
            .collect();
        clean.hash(&mut hasher);
    }
    hasher.finish()
}

pub fn format_prompt(messages: &[ChatMessage], use_markers: bool, marker_id: &str) -> String {
    let body = if messages.len() == 1 && messages[0].role.eq_ignore_ascii_case("user") {
        messages[0].content.as_plain_text().trim().to_string()
    } else {
        messages
            .iter()
            .filter_map(|m| {
                let text = m.content.as_plain_text();
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(format!("[{}]\n{}", m.role.to_ascii_uppercase(), trimmed))
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    };

    if use_markers {
        let start_tag = format!("<<<GP_START:{marker_id}>>>");
        let end_tag = format!("<<<GP_END:{marker_id}>>>");
        format!(
            "Please answer the request below and wrap your final answer strictly between {start_tag} and {end_tag}.\n\n{body}"
        )
    } else {
        body
    }
}

fn generate_marker_id() -> String {
    let mut bytes = [0u8; 6];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

struct NativeTranscriptTracker {
    path: PathBuf,
    baseline_lines: usize,
}

impl NativeTranscriptTracker {
    #[cfg(target_os = "linux")]
    fn probe(session_id: &str, agent: DetectedAgent) -> Option<Self> {
        if agent != DetectedAgent::Agy {
            return None;
        }
        let pane_pid = tmux_pane_pid(session_id).ok().flatten()?;
        for pid in read_descendant_pids(pane_pid) {
            let fd_dir = format!("/proc/{pid}/fd");
            let Ok(entries) = fs::read_dir(fd_dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(target) = fs::read_link(entry.path()) else {
                    continue;
                };
                if target.extension().and_then(|e| e.to_str()) == Some("lock")
                    && target
                        .parent()
                        .and_then(|p| p.file_name())
                        .and_then(|n| n.to_str())
                        == Some("presence")
                {
                    let Some(conv_id) = target.file_stem().and_then(|s| s.to_str()) else {
                        continue;
                    };
                    let Some(antigravity_root) = target.parent().and_then(|p| p.parent()) else {
                        continue;
                    };
                    let transcript = antigravity_root
                        .join("brain")
                        .join(conv_id)
                        .join(".system_generated/logs/transcript.jsonl");
                    if transcript.is_file() {
                        let baseline_lines = count_lines(&transcript);
                        return Some(Self {
                            path: transcript,
                            baseline_lines,
                        });
                    }
                }
            }
        }
        None
    }

    #[cfg(not(target_os = "linux"))]
    fn probe(_session_id: &str, _agent: DetectedAgent) -> Option<Self> {
        None
    }

    fn poll_new_assistant_reply(&self) -> Option<String> {
        let raw = fs::read_to_string(&self.path).ok()?;
        let lines: Vec<&str> = raw.lines().collect();
        if lines.len() <= self.baseline_lines {
            return None;
        }
        let mut replies = Vec::new();
        for line in &lines[self.baseline_lines..] {
            let Ok(val) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let role = val
                .get("role")
                .or_else(|| val.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if role.eq_ignore_ascii_case("assistant") || role.eq_ignore_ascii_case("model") {
                if let Some(text) = val
                    .get("text")
                    .or_else(|| val.get("content"))
                    .and_then(Value::as_str)
                {
                    let t = text.trim();
                    if !t.is_empty() {
                        replies.push(t.to_string());
                    }
                }
            }
        }
        if replies.is_empty() {
            None
        } else {
            Some(replies.join("\n\n"))
        }
    }
}

fn count_lines(path: &Path) -> usize {
    fs::read_to_string(path)
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

#[derive(Debug, Clone)]
pub struct AgentTurnOutcome {
    pub reply: String,
    pub detected_agent: DetectedAgent,
    pub detected_model: Option<String>,
    pub switched_model: Option<String>,
    pub pane_tail: String,
    pub elapsed_ms: u64,
}

pub async fn execute_agent_turn(
    session: Arc<TerminalSession>,
    messages: &[ChatMessage],
    desired_model: Option<&str>,
    allow_switch_when_unknown: bool,
    timeout: Duration,
    idle_quiesce: Duration,
    request_id: String,
) -> Result<AgentTurnOutcome> {
    if messages.is_empty() {
        anyhow::bail!("messages array must not be empty");
    }

    let _guard = session.request_lock.lock().await;
    session.set_active_request(Some(request_id));
    let _clear_active = ActiveRequestReset {
        session: &session,
    };

    let started = Instant::now();
    let use_markers = session.use_markers;
    let marker_id = generate_marker_id();
    let prompt = format_prompt(messages, use_markers, &marker_id);
    let desired_owned = desired_model.map(str::to_string);

    let (detected_agent, baseline_pane, mut detected_model, switched_model, transcript_tracker) =
        tokio::task::spawn_blocking({
            let sess = session.clone();
            move || -> Result<(
                DetectedAgent,
                String,
                Option<String>,
                Option<String>,
                Option<NativeTranscriptTracker>,
            )> {
                let agent = detect_running_agent_for_session(&sess);
                let mut pane = capture_session_plain(&sess, PANE_CAPTURE_LINES)
                    .context("failed to capture baseline terminal pane")?;
                let mut detected_m = detect_active_model_from_pane(&pane, agent);
                let mut switched = None;

                if let Some(target_model) = desired_owned.as_deref() {
                    let should_switch = match detected_m.as_deref() {
                        Some(curr) => !models_match(curr, target_model),
                        None => allow_switch_when_unknown && agent.supports_slash_model(),
                    };
                    if should_switch && agent.supports_slash_model() {
                        let cmd = format!("/model {target_model}");
                        send_session_paste(&sess, &cmd, true)?;
                        std::thread::sleep(Duration::from_millis(1200));
                        pane = capture_session_plain(&sess, PANE_CAPTURE_LINES)
                            .context("failed to re-capture pane after /model switch")?;
                        detected_m = detect_active_model_from_pane(&pane, agent)
                            .or_else(|| Some(target_model.to_string()));
                        switched = Some(target_model.to_string());
                    }
                }

                let tracker = NativeTranscriptTracker::probe(&sess.id, agent);
                Ok((agent, pane, detected_m, switched, tracker))
            }
        })
        .await
        .context("baseline capture task panicked")??;

    let baseline_seq = session.current_output_seq();
    let baseline_fp = pane_fingerprint(&baseline_pane);

    tokio::task::spawn_blocking({
        let sess = session.clone();
        let prompt_to_send = prompt.clone();
        move || send_session_paste(&sess, &prompt_to_send, true)
    })
    .await
    .context("send paste task panicked")??;

    let deadline = Instant::now() + timeout;
    let mut saw_change = false;
    let mut last_fp = baseline_fp;
    let mut last_change_at = Instant::now();
    let mut latest_pane = baseline_pane.clone();

    loop {
        if Instant::now() >= deadline {
            if saw_change {
                let partial = extract_pane_reply(
                    &baseline_pane,
                    &latest_pane,
                    &prompt,
                    use_markers,
                    &marker_id,
                );
                if !partial.is_empty() {
                    return Ok(AgentTurnOutcome {
                        reply: partial,
                        detected_agent,
                        detected_model: detected_model
                            .or_else(|| detect_active_model_from_pane(&latest_pane, detected_agent)),
                        switched_model,
                        pane_tail: tail_text(&latest_pane, 40),
                        elapsed_ms: started.elapsed().as_millis() as u64,
                    });
                }
            }
            anyhow::bail!("timed out waiting for terminal agent response after {timeout:?}");
        }

        tokio::time::sleep(POLL_INTERVAL).await;

        let pane_snapshot = tokio::task::spawn_blocking({
            let sess = session.clone();
            move || capture_session_plain(&sess, PANE_CAPTURE_LINES)
        })
        .await
        .context("pane poll task panicked")??;

        let fp = pane_fingerprint(&pane_snapshot);
        let working = pane_has_working_indicator(&pane_snapshot);
        let seq_changed = session.current_output_seq() > baseline_seq;

        if fp != baseline_fp || seq_changed {
            if !saw_change {
                saw_change = true;
                last_change_at = Instant::now();
            }
        }

        if fp != last_fp {
            last_fp = fp;
            last_change_at = Instant::now();
        }
        latest_pane = pane_snapshot;
        if detected_model.is_none() {
            detected_model = detect_active_model_from_pane(&latest_pane, detected_agent);
        }

        if saw_change && !working && last_change_at.elapsed() >= idle_quiesce {
            if let Some(ref tracker) = transcript_tracker {
                if let Some(native_reply) = tracker.poll_new_assistant_reply() {
                    return Ok(AgentTurnOutcome {
                        reply: native_reply,
                        detected_agent,
                        detected_model,
                        switched_model,
                        pane_tail: tail_text(&latest_pane, 40),
                        elapsed_ms: started.elapsed().as_millis() as u64,
                    });
                }
            }

            let extracted = extract_pane_reply(
                &baseline_pane,
                &latest_pane,
                &prompt,
                use_markers,
                &marker_id,
            );
            if !extracted.is_empty() {
                return Ok(AgentTurnOutcome {
                    reply: extracted,
                    detected_agent,
                    detected_model,
                    switched_model,
                    pane_tail: tail_text(&latest_pane, 40),
                    elapsed_ms: started.elapsed().as_millis() as u64,
                });
            }
        }
    }
}

fn tail_text(pane: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = pane.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

struct ActiveRequestReset<'a> {
    session: &'a TerminalSession,
}

impl Drop for ActiveRequestReset<'_> {
    fn drop(&mut self) {
        self.session.set_active_request(None);
    }
}

/// Extracts the clean assistant reply from the difference between baseline and final pane text.
pub fn extract_pane_reply(
    baseline_pane: &str,
    final_pane: &str,
    sent_prompt: &str,
    use_markers: bool,
    marker_id: &str,
) -> String {
    let base_lines = trimmed_lines(baseline_pane);
    let final_lines = trimmed_lines(final_pane);

    let common_prefix = longest_common_prefix_len(&base_lines, &final_lines);
    let mut new_lines: Vec<&str> = final_lines[common_prefix..].to_vec();

    if use_markers {
        let start_tag = format!("<<<GP_START:{marker_id}>>>");
        let end_tag = format!("<<<GP_END:{marker_id}>>>");
        let joined_new = new_lines.join("\n");
        if let Some(extracted) = extract_between_last_markers(&joined_new, &start_tag, &end_tag) {
            return extracted;
        }
    }

    let trailing_prompt = base_lines.last().map(|s| s.trim()).unwrap_or("");
    let sent_lines: Vec<&str> = sent_prompt
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();

    if !sent_lines.is_empty() {
        let first_sent = sent_lines[0];
        if let Some(first_new) = new_lines.first().map(|s| s.trim()) {
            if is_echoed_prompt_line(first_new, first_sent, trailing_prompt) {
                new_lines.remove(0);
                for sent_line in sent_lines.iter().skip(1) {
                    if new_lines
                        .first()
                        .is_some_and(|l| l.trim() == *sent_line || l.trim().ends_with(sent_line))
                    {
                        new_lines.remove(0);
                    } else {
                        break;
                    }
                }
            }
        }
    }

    // Strip any TUI footer/chrome lines from the bottom.
    while let Some(last) = new_lines.last().map(|s| s.trim()) {
        if last.is_empty()
            || (!trailing_prompt.is_empty() && last == trailing_prompt)
            || is_generic_prompt_line(last)
            || is_tui_footer_line(last)
        {
            new_lines.pop();
        } else {
            break;
        }
    }

    // Also strip leading TUI border/prompt lines if the composer redrew above the reply.
    while let Some(first) = new_lines.first().map(|s| s.trim()) {
        if first.is_empty() || is_generic_prompt_line(first) || is_tui_footer_line(first) {
            new_lines.remove(0);
        } else {
            break;
        }
    }

    // Filter out any residual TUI chrome lines in the middle and clean leading `● ` bullet.
    let cleaned: Vec<String> = new_lines
        .into_iter()
        .map(str::trim_end)
        .filter(|l| {
            let t = l.trim();
            !is_tui_footer_line(t) && !is_generic_prompt_line(t)
        })
        .enumerate()
        .map(|(idx, line)| {
            if idx == 0 {
                line.trim_start()
                    .strip_prefix("● ")
                    .map(|rest| rest.to_string())
                    .unwrap_or_else(|| line.to_string())
            } else {
                line.to_string()
            }
        })
        .collect();

    cleaned.join("\n").trim().to_string()
}

fn trimmed_lines(pane: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = pane.lines().map(str::trim_end).collect();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    lines
}

fn longest_common_prefix_len(a: &[&str], b: &[&str]) -> usize {
    let mut idx = 0;
    while idx < a.len() && idx < b.len() && a[idx] == b[idx] {
        idx += 1;
    }
    idx
}

fn is_echoed_prompt_line(line: &str, sent_first: &str, baseline_prompt: &str) -> bool {
    if line == sent_first {
        return true;
    }
    if !baseline_prompt.is_empty() {
        if let Some(rest) = line.strip_prefix(baseline_prompt) {
            if rest.trim() == sent_first {
                return true;
            }
        }
    }
    for prefix in [">", "›", "❯", "$", "#", ">>>", "agent>"] {
        if let Some(rest) = line.strip_prefix(prefix) {
            if rest.trim() == sent_first {
                return true;
            }
        }
    }
    false
}

fn is_generic_prompt_line(line: &str) -> bool {
    matches!(line, "$" | "#" | ">" | "›" | "❯" | ">>>" | "agent>")
        || line.ends_with("$ ")
        || line.ends_with("# ")
        || line.starts_with("› Ask ")
        || line.starts_with("❯ Ask ")
        || line.starts_with("❯ Plan ")
        || (line.starts_with("❯") && line.len() <= 8)
        || (line.ends_with('$') && line.len() <= 64)
        || (line.ends_with('#') && line.len() <= 64)
}

fn is_tui_footer_line(line: &str) -> bool {
    (line.contains(" · Context ") && line.contains(" used · "))
        || line.contains("← for agents")
        || line.contains("? for shortcuts")
        || line.contains("bypass permissions")
        || line.contains("shift+tab to cycle")
        || line.contains("Thinking on (")
        || line.contains("Thinking off (")
        || line == "∴ Thought"
        || line.starts_with("✔ Worked for ")
        || line.starts_with('╭')
        || line.starts_with('╰')
        || (line.starts_with('│') && line.ends_with('│'))
        || (line.chars().all(|c| matches!(c, '─' | '━' | '-' | '╭' | '╮' | '╰' | '╯' | '┌' | '┐' | '└' | '┘' | ' '))
            && line.chars().filter(|c| !c.is_whitespace()).count() >= 6)
}

fn extract_between_last_markers(text: &str, start_tag: &str, end_tag: &str) -> Option<String> {
    let end_idx = text.rfind(end_tag)?;
    let prefix = &text[..end_idx];
    let start_idx = prefix.rfind(start_tag)?;
    let content = prefix[start_idx + start_tag.len()..].trim();
    if content.is_empty() {
        None
    } else {
        Some(content.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_clean_reply_from_interactive_agent_pane() {
        let baseline = "Python Mock Agent\nagent> \n";
        let final_pane = "Python Mock Agent\nagent> hello world\nECHO_REPLY: hello world\nagent> \n";
        let reply = extract_pane_reply(baseline, final_pane, "hello world", false, "abc");
        assert_eq!(reply, "ECHO_REPLY: hello world");
    }

    #[test]
    fn extracts_codebuddy_tui_reply_and_strips_chrome() {
        let baseline = "CodeBuddy\n──────────\n❯ \n──────────\nhy4flash-free max · ← for agents · ? for shortcuts\n";
        let final_pane = "CodeBuddy\n❯ 请只回复 OK\n\n∴ Thought\n\n● OK_CODEBUDDY_LIVE\n\n✔ Worked for 5s\n──────────\n❯ \n──────────\nhy4flash-free max · ← for agents · ? for shortcuts\nThinking on (AltT to toggle)\n";
        let reply = extract_pane_reply(baseline, final_pane, "请只回复 OK", false, "id1");
        assert_eq!(reply, "OK_CODEBUDDY_LIVE");
    }

    #[test]
    fn detects_active_model_from_codebuddy_agy_codex_claude_panes() {
        assert_eq!(
            detect_active_model_from_pane(
                "hy4flash-free max · ← for agents · ? for shortcuts",
                DetectedAgent::CodeBuddy
            )
            .as_deref(),
            Some("hy4flash-free")
        );
        assert_eq!(
            detect_active_model_from_pane(
                "? for shortcuts · gemini-3.8-flash-n · medium",
                DetectedAgent::Agy
            )
            .as_deref(),
            Some("gemini-3.8-flash-n")
        );
        assert_eq!(
            detect_active_model_from_pane(
                "gpt-5.4 high · Context 15% used · Fast",
                DetectedAgent::Codex
            )
            .as_deref(),
            Some("gpt-5.4")
        );
        assert_eq!(
            detect_active_model_from_pane("Model: claude-sonnet-4-6", DetectedAgent::Claude)
                .as_deref(),
            Some("claude-sonnet-4-6")
        );
    }

    #[test]
    fn detects_quota_exhaustion_phrases() {
        assert!(pane_has_quota_exhausted("错误：当前账号积分不足，请充值").is_some());
        assert!(pane_has_quota_exhausted("Error: Quota exceeded on this model").is_some());
        assert!(pane_has_quota_exhausted("Insufficient balance").is_some());
        assert!(pane_has_quota_exhausted("OK_CODEBUDDY_LIVE").is_none());
    }

    #[test]
    fn detects_working_indicators_accurately() {
        assert!(pane_has_working_indicator("Output line\n⠋ Thinking…\n"));
        assert!(pane_has_working_indicator(
            "Working (2s • esc to interrupt)\n"
        ));
        assert!(pane_has_working_indicator("✻ Tallying tokens…\n"));
        assert!(!pane_has_working_indicator("ECHO_REPLY: done\nagent> \n"));
        assert!(!pane_has_working_indicator("› Ask Codex to do anything\n"));
    }
}
