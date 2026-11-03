//! Shared code for glidekm: wire protocol, keymap, encrypted session, config, logging.

pub mod config;
pub mod event;
pub mod ipc;
pub mod keymap;
pub mod log;
pub mod session;
pub mod update;

pub const DEFAULT_PORT: u16 = 4242;
/// Server sends a heartbeat this often.
pub const HEARTBEAT_MS: u64 = 200;
/// Server considers the client gone after this long without a Hello.
pub const CLIENT_TIMEOUT_MS: u64 = 3000;
/// Client releases everything after this long without any packet.
pub const SERVER_TIMEOUT_MS: u64 = 1000;
