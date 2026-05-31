use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::error::BridgeError;
use crate::types::{Credentials, IncomingMessage};

/// Trait abstracting a messaging platform client (Slack, Teams, etc.)
#[async_trait]
pub trait MessagingClient: Send + Sync {
    /// Connect to the messaging platform with the given credentials.
    async fn connect(&mut self, credentials: Credentials) -> Result<(), BridgeError>;

    /// Disconnect from the platform.
    async fn disconnect(&mut self) -> Result<(), BridgeError>;

    /// Send a new message to a channel. Returns the message ID/timestamp.
    async fn send_message(&self, channel: &str, content: &str) -> Result<String, BridgeError>;

    /// Edit an existing message by its ID.
    async fn edit_message(
        &self,
        channel: &str,
        message_id: &str,
        content: &str,
    ) -> Result<(), BridgeError>;

    /// Subscribe to incoming messages. Returns a receiver for messages.
    async fn subscribe(
        &mut self,
        channel: &str,
    ) -> Result<mpsc::Receiver<IncomingMessage>, BridgeError>;
}
