use std::time::Duration;
#[cfg(not(windows))]
use std::{
    io::Read,
    path::Path,
    process::{Command, Output, Stdio},
    thread,
    time::Instant,
};

#[cfg(not(windows))]
use anyhow::{Context, Result};
#[cfg(windows)]
use anyhow::Result;
#[cfg(not(windows))]
use tracing::warn;

#[cfg(not(windows))]
use super::input_pacing::{ERASE_KEY_WRITE_GAP, erase_key_segments};

#[allow(dead_code)]
pub const TMUX_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(not(windows))]
const TMUX_HISTORY_LIMIT: &str = "50000";
#[cfg(not(windows))]
const TMUX_TERMINAL_OVERRIDES: &str = "xterm-256color:indn@:rin@";
#[cfg(not(windows))]
const TMUX_SMALL_OUTPUT_LIMIT: usize = 64 * 1024;
#[cfg(not(windows))]
const TMUX_SNAPSHOT_OUTPUT_LIMIT: usize = 4 * 1024 * 1024;
#[cfg(not(windows))]
const PASTE_BUFFER_NAME: &str = "gproxy_in";
#[cfg(not(windows))]
const PASTE_ENTER_SETTLE: Duration = Duration::from_millis(80);

pub fn tmux_socket_name(session_id: &str) -> String {
    format!("gproxy_{session_id}")
}

pub fn tmux_session_name(session_id: &str) -> String {
    format!("gproxy_{session_id}")
}

/// Normalizes multi-line text for bracketed paste into raw-mode TUI agents:
/// internal line breaks become `\r`, avoiding premature submission or dropped lines.
pub fn normalize_paste_text(text: &str) -> String {
    text.replace("\r\n", "\r").replace('\n', "\r")
}

pub fn split_tmux_input_submit_keys(data: &str) -> (&str, usize) {
    let mut text_end = data.len();
    let mut submit_count = 0;
    while text_end > 0 {
        let Some((index, ch)) = data[..text_end].char_indices().next_back() else {
            break;
        };
        if ch != '\r' && ch != '\n' {
            break;
        }
        text_end = index;
        submit_count += 1;
    }
    (&data[..text_end], submit_count)
}

#[cfg(windows)]
pub fn tmux_session_exists(_session_id: &str) -> bool {
    true
}

#[cfg(windows)]
pub fn ensure_tmux_session(
    _session_id: &str,
    _cwd: &std::path::Path,
    _startup_command: Option<&str>,
) -> Result<()> {
    Ok(())
}

#[cfg(windows)]
pub fn destroy_tmux_session(_session_id: &str) -> Result<()> {
    Ok(())
}

#[cfg(windows)]
pub fn tmux_pane_pid(_session_id: &str) -> Result<Option<u32>> {
    Ok(None)
}

#[cfg(not(windows))]
pub fn isolated_tmux_command(session_id: &str) -> Command {
    let mut cmd = Command::new("tmux");
    cmd.arg("-L").arg(tmux_socket_name(session_id));
    sanitize_tmux_command(&mut cmd);
    cmd
}

#[cfg(not(windows))]
fn sanitize_tmux_command(cmd: &mut Command) {
    // Avoid nesting inside a parent tmux session or inheriting webClx/agent vars.
    cmd.env_remove("TMUX");
    cmd.env_remove("TMUX_PANE");
}

#[cfg(not(windows))]
pub fn bounded_command_output(
    mut command: Command,
    budget: Duration,
    max_bytes: usize,
) -> Result<Output> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().context("failed to spawn command")?;
    let mut stdout = child.stdout.take().context("missing stdout pipe")?;
    let mut stderr = child.stderr.take().context("missing stderr pipe")?;

    let stdout_handle = thread::spawn(move || read_bounded(&mut stdout, max_bytes));
    let stderr_handle = thread::spawn(move || read_bounded(&mut stderr, max_bytes));

    let deadline = Instant::now() + budget;
    let status = loop {
        if let Some(status) = child.try_wait().context("try_wait failed")? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_handle.join();
            let _ = stderr_handle.join();
            anyhow::bail!("command timed out after {:?}", budget);
        }
        thread::sleep(Duration::from_millis(10));
    };

    let stdout_bytes = stdout_handle
        .join()
        .map_err(|_| anyhow::anyhow!("stdout reader panicked"))??;
    let stderr_bytes = stderr_handle
        .join()
        .map_err(|_| anyhow::anyhow!("stderr reader panicked"))??;

    Ok(Output {
        status,
        stdout: stdout_bytes,
        stderr: stderr_bytes,
    })
}

#[cfg(not(windows))]
fn read_bounded<R: Read>(reader: &mut R, max_bytes: usize) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        if buf.len() < max_bytes {
            let remaining = max_bytes - buf.len();
            buf.extend_from_slice(&chunk[..n.min(remaining)]);
        }
    }
    Ok(buf)
}

#[cfg(not(windows))]
fn tmux_missing_session_error(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("no server running")
        || lower.contains("can't find session")
        || lower.contains("session not found")
        || lower.contains("no such file or directory")
        || lower.contains("connection refused")
}

#[cfg(not(windows))]
pub fn tmux_session_exists(session_id: &str) -> bool {
    let mut cmd = isolated_tmux_command(session_id);
    cmd.args(["has-session", "-t", &tmux_session_name(session_id)]);
    match bounded_command_output(cmd, TMUX_COMMAND_TIMEOUT, TMUX_SMALL_OUTPUT_LIMIT) {
        Ok(out) => out.status.success(),
        Err(err) => {
            warn!(session_id, "tmux has-session failed: {err:#}");
            false
        }
    }
}

#[cfg(not(windows))]
pub fn ensure_tmux_session(
    session_id: &str,
    cwd: &Path,
    startup_command: Option<&str>,
) -> Result<()> {
    if tmux_session_exists(session_id) {
        configure_tmux_session(session_id)?;
        return Ok(());
    }

    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "/bin/bash".to_string());

    let effective_cwd = if cwd.is_dir() {
        cwd.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_else(|_| Path::new("/").to_path_buf())
    };

    let mut cmd = isolated_tmux_command(session_id);
    cmd.arg("new-session")
        .arg("-d")
        .arg("-s")
        .arg(tmux_session_name(session_id))
        .arg("-c")
        .arg(&effective_cwd)
        .arg(&shell)
        .arg("-l");

    let output = bounded_command_output(cmd, TMUX_COMMAND_TIMEOUT, TMUX_SMALL_OUTPUT_LIMIT)
        .context("failed to create tmux session")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!("tmux new-session failed: {stderr}");
    }

    configure_tmux_session(session_id)?;

    if let Some(startup) = startup_command.map(str::trim).filter(|s| !s.is_empty()) {
        // Wait until the login shell has initialized and drawn its prompt before pasting startup.
        let shell_deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < shell_deadline {
            if let Ok(pane) = capture_pane_plain(session_id, 50) {
                if !pane.trim().is_empty() {
                    break;
                }
            }
            thread::sleep(Duration::from_millis(80));
        }
        thread::sleep(Duration::from_millis(100));
        send_tmux_paste(session_id, startup, true)?;
    }

    Ok(())
}

#[cfg(not(windows))]
fn configure_tmux_session(session_id: &str) -> Result<()> {
    let target = tmux_session_name(session_id);
    for (opt, val) in [
        ("history-limit", TMUX_HISTORY_LIMIT),
        ("status", "off"),
        ("mouse", "on"),
        ("focus-events", "on"),
        ("terminal-overrides", TMUX_TERMINAL_OVERRIDES),
    ] {
        let mut cmd = isolated_tmux_command(session_id);
        cmd.args(["set-option", "-t", &target, opt, val]);
        let out = bounded_command_output(cmd, TMUX_COMMAND_TIMEOUT, TMUX_SMALL_OUTPUT_LIMIT)?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            warn!(session_id, opt, "tmux set-option failed: {stderr}");
        }
    }

    let mut esc_cmd = isolated_tmux_command(session_id);
    esc_cmd.args(["set-option", "-sg", "escape-time", "0"]);
    let _ = bounded_command_output(esc_cmd, TMUX_COMMAND_TIMEOUT, TMUX_SMALL_OUTPUT_LIMIT);
    Ok(())
}

#[cfg(not(windows))]
pub fn destroy_tmux_session(session_id: &str) -> Result<()> {
    let mut kill_session = isolated_tmux_command(session_id);
    kill_session.args(["kill-session", "-t", &tmux_session_name(session_id)]);
    let out = bounded_command_output(kill_session, TMUX_COMMAND_TIMEOUT, TMUX_SMALL_OUTPUT_LIMIT)
        .context("failed to kill tmux session")?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() && !tmux_missing_session_error(&stderr) {
        anyhow::bail!("tmux kill-session failed: {}", stderr.trim());
    }

    let mut kill_server = isolated_tmux_command(session_id);
    kill_server.arg("kill-server");
    let _ = bounded_command_output(kill_server, TMUX_COMMAND_TIMEOUT, TMUX_SMALL_OUTPUT_LIMIT);
    Ok(())
}

#[cfg(not(windows))]
pub fn tmux_pane_pid(session_id: &str) -> Result<Option<u32>> {
    let mut cmd = isolated_tmux_command(session_id);
    cmd.args([
        "display-message",
        "-p",
        "-t",
        &tmux_session_name(session_id),
        "#{pane_pid}",
    ]);
    let out = bounded_command_output(cmd, TMUX_COMMAND_TIMEOUT, TMUX_SMALL_OUTPUT_LIMIT)
        .context("failed to read tmux pane_pid")?;
    if !out.status.success() {
        return Ok(None);
    }
    let pid = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u32>()
        .ok();
    Ok(pid)
}

/// Captures ANSI-styled pane replay bytes for xterm.js initialization.
#[cfg(not(windows))]
pub fn capture_pane_replay(session_id: &str, lines: u32) -> Result<Vec<u8>> {
    let target = tmux_session_name(session_id);
    let mut cmd = isolated_tmux_command(session_id);
    cmd.args([
        "capture-pane",
        "-e",
        "-p",
        "-S",
        &format!("-{lines}"),
        "-t",
        &target,
    ]);
    let out = bounded_command_output(cmd, TMUX_COMMAND_TIMEOUT, TMUX_SNAPSHOT_OUTPUT_LIMIT)
        .context("failed to capture tmux replay")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        anyhow::bail!("capture-pane replay failed: {stderr}");
    }
    Ok(normalize_replay_bytes(&out.stdout))
}

#[cfg(not(windows))]
fn normalize_replay_bytes(raw: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(raw);
    let trimmed = text.trim_end_matches('\n');
    let mut out = String::with_capacity(trimmed.len() + 64);
    for (idx, line) in trimmed.split('\n').enumerate() {
        if idx > 0 {
            out.push_str("\r\n");
        }
        out.push_str(line.trim_end_matches('\r'));
    }
    out.into_bytes()
}

/// Captures plain-text pane history with wrapped lines joined (`-J`).
#[cfg(not(windows))]
pub fn capture_pane_plain(session_id: &str, lines: u32) -> Result<String> {
    let target = tmux_session_name(session_id);
    let mut cmd = isolated_tmux_command(session_id);
    cmd.args([
        "capture-pane",
        "-J",
        "-p",
        "-S",
        &format!("-{lines}"),
        "-t",
        &target,
    ]);
    let out = bounded_command_output(cmd, TMUX_COMMAND_TIMEOUT, TMUX_SNAPSHOT_OUTPUT_LIMIT)
        .context("failed to capture tmux plain pane")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        anyhow::bail!("capture-pane plain failed: {stderr}");
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Sends text via tmux paste-buffer (safe for large prompts and multi-line inputs),
/// optionally followed by a `C-m` Enter keystroke.
#[cfg(not(windows))]
pub fn send_tmux_paste(session_id: &str, text: &str, send_enter: bool) -> Result<()> {
    let target = tmux_session_name(session_id);
    let normalized = normalize_paste_text(text);
    if !normalized.is_empty() {
        let mut set_cmd = isolated_tmux_command(session_id);
        set_cmd.args(["set-buffer", "-b", PASTE_BUFFER_NAME, "--", &normalized]);
        let set_out =
            bounded_command_output(set_cmd, TMUX_COMMAND_TIMEOUT, TMUX_SMALL_OUTPUT_LIMIT)
                .context("failed to set tmux paste buffer")?;
        if !set_out.status.success() {
            let stderr = String::from_utf8_lossy(&set_out.stderr).trim().to_string();
            anyhow::bail!("tmux set-buffer failed: {stderr}");
        }

        let mut paste_cmd = isolated_tmux_command(session_id);
        paste_cmd.args([
            "paste-buffer",
            "-p",
            "-d",
            "-b",
            PASTE_BUFFER_NAME,
            "-t",
            &target,
        ]);
        let paste_out =
            bounded_command_output(paste_cmd, TMUX_COMMAND_TIMEOUT, TMUX_SMALL_OUTPUT_LIMIT)
                .context("failed to paste tmux buffer")?;
        if !paste_out.status.success() {
            let stderr = String::from_utf8_lossy(&paste_out.stderr)
                .trim()
                .to_string();
            anyhow::bail!("tmux paste-buffer failed: {stderr}");
        }
    }

    if send_enter {
        if !normalized.is_empty() {
            thread::sleep(PASTE_ENTER_SETTLE);
        }
        send_tmux_enter(session_id)?;
    }

    Ok(())
}

#[cfg(not(windows))]
pub fn send_tmux_enter(session_id: &str) -> Result<()> {
    send_tmux_keys(session_id, &["C-m"])
}

#[cfg(not(windows))]
#[allow(dead_code)]
pub fn send_tmux_input(session_id: &str, data: &str) -> Result<()> {
    let (text, submit_count) = split_tmux_input_submit_keys(data);
    for (index, segment) in erase_key_segments(text).into_iter().enumerate() {
        if index > 0 {
            thread::sleep(ERASE_KEY_WRITE_GAP);
        }
        send_tmux_literal_keys(session_id, segment)?;
    }
    for _ in 0..submit_count {
        send_tmux_keys(session_id, &["C-m"])?;
    }
    Ok(())
}

#[cfg(not(windows))]
fn send_tmux_literal_keys(session_id: &str, text: &str) -> Result<()> {
    let mut cmd = isolated_tmux_command(session_id);
    cmd.args([
        "send-keys",
        "-t",
        &tmux_session_name(session_id),
        "-l",
        "--",
        text,
    ]);
    let out = bounded_command_output(cmd, TMUX_COMMAND_TIMEOUT, TMUX_SMALL_OUTPUT_LIMIT)
        .context("failed to send literal keys")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        anyhow::bail!("tmux send-keys -l failed: {stderr}");
    }
    Ok(())
}

#[cfg(not(windows))]
fn send_tmux_keys(session_id: &str, keys: &[&str]) -> Result<()> {
    let mut cmd = isolated_tmux_command(session_id);
    cmd.arg("send-keys")
        .arg("-t")
        .arg(tmux_session_name(session_id))
        .args(keys);
    let out = bounded_command_output(cmd, TMUX_COMMAND_TIMEOUT, TMUX_SMALL_OUTPUT_LIMIT)
        .context("failed to send keys")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        anyhow::bail!("tmux send-keys failed: {stderr}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_and_session_names_are_isolated() {
        assert_eq!(tmux_socket_name("abc"), "gproxy_abc");
        assert_eq!(tmux_session_name("abc"), "gproxy_abc");
    }

    #[test]
    fn split_submit_keys_counts_trailing_cr_lf() {
        assert_eq!(split_tmux_input_submit_keys("hello\r\n"), ("hello", 2));
        assert_eq!(split_tmux_input_submit_keys("hi\r"), ("hi", 1));
        assert_eq!(split_tmux_input_submit_keys("plain"), ("plain", 0));
    }

    #[test]
    fn normalize_paste_text_converts_newlines_to_cr() {
        assert_eq!(normalize_paste_text("a\r\nb\nc"), "a\rb\rc");
    }
}
