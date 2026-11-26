pub mod account_rotation;
pub mod agent;
pub mod input_pacing;
pub mod manager;
pub mod session;
pub mod tmux;
pub mod websocket;

pub const DEFAULT_COLS: u16 = 120;
pub const DEFAULT_ROWS: u16 = 36;
pub const MAX_BACKLOG_BYTES: usize = 512 * 1024;
pub const MAX_INITIAL_REPLAY_BYTES: usize = 256 * 1024;
pub const INITIAL_SNAPSHOT_LINE_LIMIT: u32 = 1000;
