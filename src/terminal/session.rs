use std::{
    collections::VecDeque,
    io::{Read, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex as AsyncMutex, broadcast};
use tracing::warn;

use super::{DEFAULT_COLS, DEFAULT_ROWS, MAX_BACKLOG_BYTES, input_pacing::write_input_paced};

const TERMINAL_OUTPUT_CHANNEL_CAPACITY: usize = 2048;
const MAX_PLAIN_TEXT_BUFFER_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    #[default]
    Auto,
    Agy,
    Codex,
    Claude,
    #[serde(rename = "codebuddy")]
    CodeBuddy,
    Generic,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConversationState {
    #[serde(default)]
    pub history_hashes: Vec<String>,
    #[serde(default)]
    pub response_ids: Vec<String>,
    #[serde(default)]
    pub explicit_conversation_id: Option<String>,
    #[serde(default)]
    pub turn_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredSessionMeta {
    pub id: String,
    pub name: String,
    pub model_alias: String,
    pub cwd: PathBuf,
    #[serde(default)]
    pub startup_command: Option<String>,
    #[serde(default)]
    pub agent_kind: AgentKind,
    #[serde(default)]
    pub use_markers: bool,
    #[serde(default)]
    pub owner_key_id: Option<String>,
    #[serde(default)]
    pub managed: bool,
    #[serde(default)]
    pub conversation_state: ConversationState,
    pub created_at: u64,
    #[serde(default)]
    pub last_used_at: u64,
}

#[derive(Clone, Debug)]
pub struct TerminalOutputChunk {
    pub seq: u64,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
pub enum TerminalStreamEvent {
    Output(TerminalOutputChunk),
}

#[derive(Debug, Default)]
pub struct TerminalOutputBacklog {
    chunks: VecDeque<TerminalOutputChunk>,
    total_bytes: usize,
}

impl TerminalOutputBacklog {
    pub fn push(&mut self, chunk: TerminalOutputChunk, max_bytes: usize) {
        self.total_bytes = self.total_bytes.saturating_add(chunk.bytes.len());
        self.chunks.push_back(chunk);
        while self.total_bytes > max_bytes {
            let Some(front) = self.chunks.pop_front() else {
                self.total_bytes = 0;
                break;
            };
            self.total_bytes = self.total_bytes.saturating_sub(front.bytes.len());
        }
    }

    pub fn chunks_after(&self, seq: u64) -> Vec<TerminalOutputChunk> {
        self.chunks
            .iter()
            .filter(|chunk| chunk.seq > seq)
            .cloned()
            .collect()
    }

    pub fn tail_snapshot(&self, max_bytes: usize) -> Vec<u8> {
        let mut collected: Vec<&[u8]> = Vec::new();
        let mut remaining = max_bytes;
        for chunk in self.chunks.iter().rev() {
            if remaining == 0 {
                break;
            }
            if chunk.bytes.len() <= remaining {
                collected.push(&chunk.bytes);
                remaining -= chunk.bytes.len();
            } else {
                let offset = chunk.bytes.len() - remaining;
                collected.push(&chunk.bytes[offset..]);
                remaining = 0;
            }
        }
        let mut out = Vec::with_capacity(max_bytes.min(self.total_bytes));
        for slice in collected.into_iter().rev() {
            out.extend_from_slice(slice);
        }
        out
    }
}

pub struct TerminalSession {
    pub id: String,
    pub name: String,
    pub model_alias: String,
    pub cwd: PathBuf,
    pub startup_command: Option<String>,
    pub agent_kind: AgentKind,
    pub use_markers: bool,
    pub owner_key_id: Option<String>,
    pub managed: bool,
    pub created_at: u64,
    last_used_at: AtomicU64,
    conversation_state: RwLock<ConversationState>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    broadcaster: broadcast::Sender<TerminalStreamEvent>,
    backlog: Mutex<TerminalOutputBacklog>,
    plain_text_buffer: Mutex<String>,
    next_output_seq: AtomicU64,
    last_output_at: AtomicU64,
    alive: AtomicBool,
    pub request_lock: AsyncMutex<()>,
    pub active_request_id: RwLock<Option<String>>,
}

impl TerminalSession {
    pub fn attach(meta: StoredSessionMeta) -> Result<Arc<Self>> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: DEFAULT_ROWS,
                cols: DEFAULT_COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to open PTY")?;

        let attach_cwd = if meta.cwd.is_dir() {
            meta.cwd.clone()
        } else {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
        };

        let child = spawn_pty_child(pair.slave.as_ref(), &meta, &attach_cwd)?;
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .context("failed to clone PTY reader")?;
        let writer = pair
            .master
            .take_writer()
            .context("failed to open PTY writer")?;

        let (broadcaster, _) = broadcast::channel(TERMINAL_OUTPUT_CHANNEL_CAPACITY);
        let now = now_millis();
        let last_used = if meta.last_used_at == 0 {
            now
        } else {
            meta.last_used_at
        };

        let session = Arc::new(Self {
            id: meta.id,
            name: meta.name,
            model_alias: meta.model_alias,
            cwd: meta.cwd,
            startup_command: meta.startup_command.clone(),
            agent_kind: meta.agent_kind,
            use_markers: meta.use_markers,
            owner_key_id: meta.owner_key_id,
            managed: meta.managed,
            created_at: meta.created_at,
            last_used_at: AtomicU64::new(last_used),
            conversation_state: RwLock::new(meta.conversation_state),
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            broadcaster,
            backlog: Mutex::new(TerminalOutputBacklog::default()),
            plain_text_buffer: Mutex::new(String::new()),
            next_output_seq: AtomicU64::new(0),
            last_output_at: AtomicU64::new(now),
            alive: AtomicBool::new(true),
            request_lock: AsyncMutex::new(()),
            active_request_id: RwLock::new(None),
        });

        Self::spawn_reader_thread(session.clone(), reader);

        #[cfg(windows)]
        if let Some(startup) = meta
            .startup_command
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let startup_cmd = startup.to_string();
            let sess = session.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(350));
                let _ = sess.write_input(&format!("{startup_cmd}\r"));
            });
        }

        Ok(session)
    }

    fn spawn_reader_thread(session: Arc<Self>, mut reader: Box<dyn Read + Send>) {
        std::thread::Builder::new()
            .name(format!("pty-reader-{}", session.id))
            .spawn(move || {
                let mut buffer = [0u8; 8192];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => {
                            session.alive.store(false, Ordering::Release);
                            break;
                        }
                        Ok(n) => {
                            let bytes = &buffer[..n];
                            let seq = session.next_output_seq.fetch_add(1, Ordering::AcqRel) + 1;
                            session.last_output_at.store(now_millis(), Ordering::Release);
                            let chunk = TerminalOutputChunk {
                                seq,
                                bytes: bytes.to_vec(),
                            };
                            if let Ok(mut backlog) = session.backlog.lock() {
                                backlog.push(chunk.clone(), MAX_BACKLOG_BYTES);
                            }
                            let clean = strip_ansi_escape_sequences(bytes);
                            if !clean.is_empty() {
                                if let Ok(mut plain) = session.plain_text_buffer.lock() {
                                    plain.push_str(&clean);
                                    if plain.len() > MAX_PLAIN_TEXT_BUFFER_BYTES {
                                        let excess = plain.len() - MAX_PLAIN_TEXT_BUFFER_BYTES;
                                        let mut cut = excess;
                                        while cut < plain.len() && !plain.is_char_boundary(cut) {
                                            cut += 1;
                                        }
                                        plain.drain(..cut);
                                    }
                                }
                            }
                            let _ = session.broadcaster.send(TerminalStreamEvent::Output(chunk));
                        }
                        Err(err) => {
                            if err.kind() == std::io::ErrorKind::Interrupted {
                                continue;
                            }
                            warn!(session_id = %session.id, "PTY reader stopped: {err}");
                            session.alive.store(false, Ordering::Release);
                            break;
                        }
                    }
                }
            })
            .ok();
    }

    pub fn to_meta(&self) -> StoredSessionMeta {
        StoredSessionMeta {
            id: self.id.clone(),
            name: self.name.clone(),
            model_alias: self.model_alias.clone(),
            cwd: self.cwd.clone(),
            startup_command: self.startup_command.clone(),
            agent_kind: self.agent_kind,
            use_markers: self.use_markers,
            owner_key_id: self.owner_key_id.clone(),
            managed: self.managed,
            conversation_state: self
                .conversation_state
                .read()
                .map(|g| g.clone())
                .unwrap_or_default(),
            created_at: self.created_at,
            last_used_at: self.last_used_at.load(Ordering::Acquire),
        }
    }

    pub fn plain_text_tail(&self, max_lines: u32) -> String {
        let Ok(guard) = self.plain_text_buffer.lock() else {
            return String::new();
        };
        let all_lines: Vec<&str> = guard.lines().collect();
        let limit = max_lines as usize;
        let start = all_lines.len().saturating_sub(limit);
        all_lines[start..].join("\n")
    }

    pub fn raw_backlog_snapshot(&self, max_bytes: usize) -> Vec<u8> {
        self.backlog
            .lock()
            .map(|b| b.tail_snapshot(max_bytes))
            .unwrap_or_default()
    }

    pub fn touch_last_used(&self) {
        self.last_used_at.store(now_millis(), Ordering::Release);
    }

    pub fn last_used_at_ms(&self) -> u64 {
        self.last_used_at.load(Ordering::Acquire)
    }

    pub fn last_history_hash(&self) -> Option<String> {
        self.conversation_state
            .read()
            .ok()
            .and_then(|g| g.history_hashes.last().cloned())
    }

    pub fn has_response_id(&self, resp_id: &str) -> bool {
        self.conversation_state
            .read()
            .map(|g| g.response_ids.iter().any(|id| id == resp_id))
            .unwrap_or(false)
    }

    pub fn explicit_conversation_id(&self) -> Option<String> {
        self.conversation_state
            .read()
            .ok()
            .and_then(|g| g.explicit_conversation_id.clone())
    }

    pub fn turn_count(&self) -> usize {
        self.conversation_state
            .read()
            .map(|g| g.turn_count)
            .unwrap_or(0)
    }

    pub fn record_turn(
        &self,
        history_hash: String,
        response_id: Option<String>,
        explicit_conv_id: Option<String>,
    ) {
        self.touch_last_used();
        if let Ok(mut guard) = self.conversation_state.write() {
            guard.history_hashes.push(history_hash);
            if guard.history_hashes.len() > 64 {
                guard.history_hashes.remove(0);
            }
            if let Some(rid) = response_id {
                guard.response_ids.push(rid);
                if guard.response_ids.len() > 64 {
                    guard.response_ids.remove(0);
                }
            }
            if guard.explicit_conversation_id.is_none() {
                guard.explicit_conversation_id = explicit_conv_id;
            }
            guard.turn_count += 1;
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<TerminalStreamEvent> {
        self.broadcaster.subscribe()
    }

    pub fn current_output_seq(&self) -> u64 {
        self.next_output_seq.load(Ordering::Acquire)
    }

    pub fn last_output_at_ms(&self) -> u64 {
        self.last_output_at.load(Ordering::Acquire)
    }

    pub fn is_alive(&self) -> bool {
        if !self.alive.load(Ordering::Acquire) {
            return false;
        }
        if let Ok(mut child) = self.child.try_lock() {
            if let Ok(Some(_)) = child.try_wait() {
                self.alive.store(false, Ordering::Release);
                return false;
            }
        }
        true
    }

    pub fn write_input(&self, data: &str) -> Result<()> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("PTY writer lock poisoned"))?;
        write_input_paced(&mut *writer, data).context("failed to write to terminal PTY")
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let master = self
            .master
            .lock()
            .map_err(|_| anyhow::anyhow!("PTY master lock poisoned"))?;
        master
            .resize(PtySize {
                rows: rows.max(2),
                cols: cols.max(2),
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to resize PTY")
    }

    pub fn chunks_after(&self, seq: u64) -> Vec<TerminalOutputChunk> {
        self.backlog
            .lock()
            .map(|b| b.chunks_after(seq))
            .unwrap_or_default()
    }

    pub fn detach_client(&self) {
        self.alive.store(false, Ordering::Release);
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    pub fn is_busy(&self) -> bool {
        self.active_request_id
            .read()
            .map(|g| g.is_some())
            .unwrap_or(false)
    }

    pub fn set_active_request(&self, req_id: Option<String>) {
        if let Ok(mut guard) = self.active_request_id.write() {
            *guard = req_id;
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(not(windows))]
fn spawn_pty_child(
    slave: &dyn portable_pty::SlavePty,
    meta: &StoredSessionMeta,
    attach_cwd: &PathBuf,
) -> Result<Box<dyn portable_pty::Child + Send + Sync>> {
    let mut cmd = CommandBuilder::new("tmux");
    cmd.arg("-L");
    cmd.arg(super::tmux::tmux_socket_name(&meta.id));
    cmd.arg("attach-session");
    cmd.arg("-t");
    cmd.arg(super::tmux::tmux_session_name(&meta.id));
    cmd.cwd(attach_cwd);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env_remove("TMUX");
    cmd.env_remove("TMUX_PANE");
    slave
        .spawn_command(cmd)
        .context("failed to attach tmux session over PTY")
}

#[cfg(windows)]
fn spawn_pty_child(
    slave: &dyn portable_pty::SlavePty,
    _meta: &StoredSessionMeta,
    attach_cwd: &PathBuf,
) -> Result<Box<dyn portable_pty::Child + Send + Sync>> {
    let mut ps = CommandBuilder::new("powershell.exe");
    ps.arg("-NoLogo");
    ps.cwd(attach_cwd);
    ps.env("TERM", "xterm-256color");
    ps.env("COLORTERM", "truecolor");
    match slave.spawn_command(ps) {
        Ok(child) => Ok(child),
        Err(_) => {
            let mut cmd = CommandBuilder::new("cmd.exe");
            cmd.cwd(attach_cwd);
            cmd.env("TERM", "xterm-256color");
            slave
                .spawn_command(cmd)
                .context("failed to spawn powershell.exe or cmd.exe in ConPTY")
        }
    }
}

/// Cross-platform plain-text pane capture: uses `tmux capture-pane` on Linux/Unix
/// and the live ANSI-stripped ConPTY buffer on Windows.
pub fn capture_session_plain(session: &TerminalSession, lines: u32) -> Result<String> {
    #[cfg(not(windows))]
    {
        super::tmux::capture_pane_plain(&session.id, lines)
    }
    #[cfg(windows)]
    {
        Ok(session.plain_text_tail(lines))
    }
}

/// Cross-platform ANSI replay capture for WebSocket xterm.js initialization.
pub fn capture_session_replay(session: &TerminalSession, lines: u32) -> Result<Vec<u8>> {
    #[cfg(not(windows))]
    {
        super::tmux::capture_pane_replay(&session.id, lines)
    }
    #[cfg(windows)]
    {
        let _ = lines;
        Ok(session.raw_backlog_snapshot(super::MAX_INITIAL_REPLAY_BYTES))
    }
}

/// Cross-platform paste-and-submit into a session.
pub fn send_session_paste(session: &TerminalSession, text: &str, send_enter: bool) -> Result<()> {
    #[cfg(not(windows))]
    {
        super::tmux::send_tmux_paste(&session.id, text, send_enter)
    }
    #[cfg(windows)]
    {
        let normalized = super::tmux::normalize_paste_text(text);
        if !normalized.is_empty() {
            session.write_input(&normalized)?;
        }
        if send_enter {
            if !normalized.is_empty() {
                std::thread::sleep(std::time::Duration::from_millis(80));
            }
            session.write_input("\r")?;
        }
        Ok(())
    }
}

/// Strips ANSI CSI/OSC/ESC sequences from raw PTY bytes for Windows ConPTY plain-text diffing.
pub fn strip_ansi_escape_sequences(input: &[u8]) -> String {
    let text = String::from_utf8_lossy(input);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => break,
            },
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                    out.push('\n');
                }
            }
            c if c.is_control() && c != '\n' && c != '\t' => {}
            c => out.push(c),
        }
    }
    out
}

pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_csi_osc_and_normalizes_crlf() {
        let raw = b"\x1b[32mHello\x1b[0m \x1b]0;title\x07world\r\nNext line\x1b[1K";
        assert_eq!(strip_ansi_escape_sequences(raw), "Hello world\nNext line");
    }
}
