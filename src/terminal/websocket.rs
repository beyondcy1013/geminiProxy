use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::broadcast;
use tracing::warn;

use super::{
    INITIAL_SNAPSHOT_LINE_LIMIT, MAX_INITIAL_REPLAY_BYTES,
    session::{TerminalSession, TerminalStreamEvent, capture_session_replay},
};

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientWsMessage {
    Input { data: String },
    Resize { cols: u16, rows: u16 },
    Ping,
}

pub async fn handle_terminal_socket(socket: WebSocket, session: Arc<TerminalSession>) {
    // Subscribe BEFORE capturing initial snapshot so live chunks during capture are preserved.
    let mut rx = session.subscribe();
    let seq_before = session.current_output_seq();

    let sess_replay = session.clone();
    let replay_bytes = tokio::task::spawn_blocking(move || {
        capture_session_replay(&sess_replay, INITIAL_SNAPSHOT_LINE_LIMIT)
    })
    .await
    .ok()
    .and_then(|r| r.ok())
    .unwrap_or_default();

    let (mut sender, mut receiver) = socket.split();

    let connected_msg = serde_json::json!({
        "type": "connected",
        "session_id": session.id,
        "name": session.name,
        "model_alias": session.model_alias,
    });
    if sender
        .send(Message::Text(connected_msg.to_string().into()))
        .await
        .is_err()
    {
        return;
    }

    if !replay_bytes.is_empty() {
        let start = replay_bytes.len().saturating_sub(MAX_INITIAL_REPLAY_BYTES);
        if sender
            .send(Message::Binary(replay_bytes[start..].to_vec().into()))
            .await
            .is_err()
        {
            return;
        }
    }

    // Flush any chunks produced while capture_pane_replay was running.
    let mut last_sent_seq = seq_before;
    for chunk in session.chunks_after(seq_before) {
        if chunk.seq > last_sent_seq {
            last_sent_seq = chunk.seq;
            if sender
                .send(Message::Binary(chunk.bytes.into()))
                .await
                .is_err()
            {
                return;
            }
        }
    }

    let input_session = session.clone();
    let mut input_task = tokio::spawn(async move {
        while let Some(next) = receiver.next().await {
            match next {
                Ok(Message::Text(text)) => {
                    if let Ok(msg) = serde_json::from_str::<ClientWsMessage>(&text) {
                        match msg {
                            ClientWsMessage::Input { data } => {
                                if let Err(err) = input_session.write_input(&data) {
                                    warn!(session_id = %input_session.id, "ws write_input failed: {err:#}");
                                    break;
                                }
                            }
                            ClientWsMessage::Resize { cols, rows } => {
                                if let Err(err) = input_session.resize(cols, rows) {
                                    warn!(session_id = %input_session.id, "ws resize failed: {err:#}");
                                }
                            }
                            ClientWsMessage::Ping => {}
                        }
                    }
                }
                Ok(Message::Binary(bytes)) => {
                    let text = String::from_utf8_lossy(&bytes);
                    if let Err(err) = input_session.write_input(&text) {
                        warn!(session_id = %input_session.id, "ws binary input failed: {err:#}");
                        break;
                    }
                }
                Ok(Message::Close(_)) => break,
                Ok(_) => {}
                Err(err) => {
                    warn!(session_id = %input_session.id, "ws recv error: {err}");
                    break;
                }
            }
        }
    });

    loop {
        tokio::select! {
            _ = &mut input_task => {
                break;
            }
            evt = rx.recv() => {
                match evt {
                    Ok(TerminalStreamEvent::Output(chunk)) => {
                        if chunk.seq <= last_sent_seq {
                            continue;
                        }
                        last_sent_seq = chunk.seq;
                        if sender.send(Message::Binary(chunk.bytes.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!(session_id = %session.id, skipped, "ws output lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    input_task.abort();
}
