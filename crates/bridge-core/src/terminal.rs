use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

use crate::error::BridgeError;
use crate::types::TerminalSize;

/// Trait abstracting a terminal/PTY backend.
/// Implementations handle spawning a shell and managing I/O.
#[async_trait]
pub trait TerminalBackend: Send {
    /// Spawn a new shell process with the given command and size.
    /// Returns channels for reading output and writing input.
    async fn spawn(
        &mut self,
        command: &str,
        size: TerminalSize,
    ) -> Result<TerminalHandle, BridgeError>;

    /// Resize the terminal to new dimensions.
    fn resize(&mut self, size: TerminalSize) -> Result<(), BridgeError>;

    /// Check if the child process is still running.
    fn is_alive(&self) -> bool;

    /// Kill the child process.
    fn kill(&mut self) -> Result<(), BridgeError>;
}

/// Handle returned after spawning a terminal, providing I/O channels.
pub struct TerminalHandle {
    /// Receiver for terminal output bytes
    pub output_rx: mpsc::Receiver<Vec<u8>>,
    /// Sender for terminal input bytes
    pub input_tx: mpsc::Sender<Vec<u8>>,
    /// Resolves when the child process exits. Backends should fire this as
    /// soon as the child is reaped, even if the OS hasn't closed the PTY
    /// read pipe yet (Windows ConPTY in particular can delay EOF). Carries
    /// no payload — the bridge only needs to know that the shell is done.
    pub exit_rx: oneshot::Receiver<()>,
}
