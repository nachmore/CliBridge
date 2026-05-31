use std::io::{Read, Write};

use async_trait::async_trait;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::mpsc;
use tracing::{debug, error};

use bridge_core::error::BridgeError;
use bridge_core::terminal::{TerminalBackend, TerminalHandle};
use bridge_core::types::TerminalSize;

/// PTY-based terminal backend using portable-pty (ConPTY on Windows, Unix PTY on Mac/Linux).
pub struct PtyBackend {
    master: Option<Box<dyn MasterPty + Send>>,
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
}

impl PtyBackend {
    pub fn new() -> Self {
        Self {
            master: None,
            child: None,
        }
    }
}

impl Default for PtyBackend {
    fn default() -> Self {
        Self::new()
    }
}

fn to_pty_size(size: TerminalSize) -> PtySize {
    PtySize {
        rows: size.rows,
        cols: size.cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

#[async_trait]
impl TerminalBackend for PtyBackend {
    async fn spawn(
        &mut self,
        command: &str,
        size: TerminalSize,
    ) -> Result<TerminalHandle, BridgeError> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(to_pty_size(size))
            .map_err(|e| BridgeError::Terminal(format!("Failed to open PTY: {e}")))?;

        let cmd = build_command(command);
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| BridgeError::Terminal(format!("Failed to spawn command: {e}")))?;

        // Drop the slave — we only interact via the master
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| BridgeError::Terminal(format!("Failed to clone PTY reader: {e}")))?;

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| BridgeError::Terminal(format!("Failed to take PTY writer: {e}")))?;

        self.master = Some(pair.master);
        self.child = Some(child);

        // Output channel: PTY -> application
        let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>(256);

        // Spawn a blocking thread to read from the PTY
        tokio::task::spawn_blocking(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        debug!("PTY reader: EOF");
                        break;
                    }
                    Ok(n) => {
                        if output_tx.blocking_send(buf[..n].to_vec()).is_err() {
                            debug!("PTY reader: output channel closed");
                            break;
                        }
                    }
                    Err(e) => {
                        // On Windows, ERROR_BROKEN_PIPE (109) means the child exited
                        if e.raw_os_error() == Some(109) {
                            debug!("PTY reader: child process exited");
                        } else {
                            error!("PTY reader error: {e}");
                        }
                        break;
                    }
                }
            }
        });

        // Input channel: application -> PTY
        let (input_tx, mut input_rx) = mpsc::channel::<Vec<u8>>(256);

        tokio::task::spawn_blocking(move || {
            let mut writer = writer;
            while let Some(data) = input_rx.blocking_recv() {
                if let Err(e) = writer.write_all(&data) {
                    error!("PTY writer error: {e}");
                    break;
                }
                let _ = writer.flush();
            }
            debug!("PTY writer: channel closed");
        });

        Ok(TerminalHandle {
            output_rx,
            input_tx,
        })
    }

    fn resize(&mut self, size: TerminalSize) -> Result<(), BridgeError> {
        if let Some(master) = &self.master {
            master
                .resize(to_pty_size(size))
                .map_err(|e| BridgeError::Terminal(format!("Failed to resize PTY: {e}")))?;
        }
        Ok(())
    }

    fn is_alive(&self) -> bool {
        self.child.is_some() && self.master.is_some()
    }

    fn kill(&mut self) -> Result<(), BridgeError> {
        if let Some(mut child) = self.child.take() {
            child
                .kill()
                .map_err(|e| BridgeError::Terminal(format!("Failed to kill child: {e}")))?;
        }
        self.master = None;
        Ok(())
    }
}

fn build_command(command: &str) -> CommandBuilder {
    let mut cmd = CommandBuilder::new(command);
    cmd.env("TERM", "xterm-256color");
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_spawn_and_read_output() {
        let mut backend = PtyBackend::new();
        let shell = if cfg!(windows) { "cmd.exe" } else { "/bin/sh" };

        let handle = backend
            .spawn(shell, TerminalSize::default())
            .await
            .expect("Failed to spawn");

        // Should be alive
        assert!(backend.is_alive());

        // Send a command
        handle
            .input_tx
            .send(b"echo hello\r\n".to_vec())
            .await
            .expect("Failed to send input");

        // Read some output (should contain "hello")
        let mut output = String::new();
        let mut rx = handle.output_rx;
        let timeout = tokio::time::sleep(std::time::Duration::from_secs(3));
        tokio::pin!(timeout);

        loop {
            tokio::select! {
                Some(data) = rx.recv() => {
                    output.push_str(&String::from_utf8_lossy(&data));
                    if output.contains("hello") {
                        break;
                    }
                }
                () = &mut timeout => {
                    break;
                }
            }
        }

        assert!(
            output.contains("hello"),
            "Expected 'hello' in output, got: {output}"
        );

        // Kill it
        backend.kill().expect("Failed to kill");
        assert!(!backend.is_alive());
    }

    #[tokio::test]
    async fn test_resize() {
        let mut backend = PtyBackend::new();
        let shell = if cfg!(windows) { "cmd.exe" } else { "/bin/sh" };

        let _handle = backend
            .spawn(shell, TerminalSize::default())
            .await
            .expect("Failed to spawn");

        // Resize should succeed
        backend
            .resize(TerminalSize {
                cols: 120,
                rows: 40,
            })
            .expect("Failed to resize");

        backend.kill().expect("Failed to kill");
    }
}
