use serde::{Deserialize, Serialize};

/// Terminal dimensions
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSize {
    pub cols: u16,
    pub rows: u16,
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self { cols: 80, rows: 24 }
    }
}

/// A message sent to the messaging platform
#[derive(Debug, Clone)]
pub struct OutgoingMessage {
    pub channel: String,
    pub content: String,
    pub message_id: Option<String>,
}

/// A message received from the messaging platform
#[derive(Debug, Clone)]
pub struct IncomingMessage {
    pub channel: String,
    pub text: String,
    pub user: String,
    pub timestamp: String,
}

/// Credentials for a messaging platform
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub token: String,
    pub cookie: Option<String>,
    pub workspace_url: Option<String>,
    pub workspace_name: Option<String>,
}
