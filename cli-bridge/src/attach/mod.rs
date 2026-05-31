//! Local attach: gives the user a real terminal that mirrors the bridge's
//! shell. The bridge process owns the PTY and forwards output to both Slack
//! and any connected attach client; the attach client is a separate
//! `cli-bridge --attach` invocation running in a fresh terminal window.
//!
//! See `protocol.rs` for the wire format, `server.rs` for the bridge-side
//! listener, `client.rs` for the attach-mode entrypoint, and `spawn.rs` for
//! the per-OS new-terminal-window launcher.

pub mod client;
pub mod protocol;
pub mod server;
pub mod spawn;

pub use client::run as run_attach_client;
pub use server::{AttachEvent, AttachServer};
pub use spawn::{open_attach_terminal, print_manual_attach_hint};
