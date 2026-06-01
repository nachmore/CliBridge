use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error};

use bridge_core::error::BridgeError;
use bridge_core::terminal::{TerminalBackend, TerminalHandle};
use bridge_core::types::TerminalSize;

/// How often the watcher polls `try_wait()` on the child process. Cheap; the
/// only reason it's not faster is that we don't need it to be — exit
/// detection within ~200ms is plenty.
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// PTY-based terminal backend using portable-pty (ConPTY on Windows, Unix PTY on Mac/Linux).
pub struct PtyBackend {
    master: Option<Box<dyn MasterPty + Send>>,
    /// Child process. Wrapped so a watcher task can poll try_wait() while
    /// the bridge thread can still call kill() — both need mutable access.
    child: Option<Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>>,
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
        let child = Arc::new(Mutex::new(child));
        self.child = Some(child.clone());

        // Output channel: PTY -> application
        let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>(256);

        // Exit signal: fires when the child process is reaped, even if the
        // PTY's read pipe hasn't returned EOF yet. Windows ConPTY in
        // particular can sit on a closed pipe for a beat after `exit`.
        let (exit_tx, exit_rx) = oneshot::channel::<()>();
        spawn_child_watcher(child, exit_tx);

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
                        // Treat the various "child exited / pipe closed"
                        // codes as benign — they happen on every clean exit.
                        // Anything else is a real error worth surfacing.
                        if is_child_exit_error(&e) {
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
            exit_rx,
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
        if let Some(child) = self.child.take()
            && let Ok(mut guard) = child.lock()
        {
            guard
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

/// Identify the OS-specific error codes the PTY surfaces when the child
/// exits cleanly. Treated as a normal end-of-session signal, not an error.
///
/// - Windows: ERROR_BROKEN_PIPE (109) — ConPTY closes its read end.
/// - Unix: EIO (5) is what Linux/macOS PTYs return when the slave is closed
///   and EPIPE (32) shows up on writes; either should be benign here.
///   `ErrorKind::BrokenPipe` covers EPIPE portably; raw 5 covers EIO.
fn is_child_exit_error(e: &std::io::Error) -> bool {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        return true;
    }
    match e.raw_os_error() {
        #[cfg(windows)]
        Some(109) => true,
        #[cfg(unix)]
        Some(5) => true, // EIO
        _ => false,
    }
}

/// Spawn a task that polls `try_wait()` on the child and fires `exit_tx`
/// when it returns Some(_). Held under a Mutex with the kill path; lock is
/// uncontended in steady state.
fn spawn_child_watcher(
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
    exit_tx: oneshot::Sender<()>,
) {
    tokio::task::spawn_blocking(move || {
        loop {
            std::thread::sleep(CHILD_POLL_INTERVAL);
            // If the bridge dropped its Arc clone (kill called .take()),
            // the strong count would be 1 here. We could continue watching,
            // but if the user explicitly killed, the bridge already knows.
            // Just wait for try_wait to report the exit.
            let status = match child.lock() {
                Ok(mut g) => g.try_wait(),
                Err(_) => {
                    debug!("PTY child watcher: mutex poisoned, exiting");
                    return;
                }
            };
            match status {
                Ok(Some(s)) => {
                    debug!("PTY child exited: {s:?}");
                    let _ = exit_tx.send(());
                    return;
                }
                Ok(None) => continue,
                Err(e) => {
                    debug!("PTY child try_wait error: {e}");
                    let _ = exit_tx.send(());
                    return;
                }
            }
        }
    });
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
    async fn test_exit_signal_fires_on_kill() {
        // After we kill the child, exit_rx must resolve. This is what the
        // bridge relies on to detect "shell exited" without depending on
        // the PTY's read pipe to return EOF (Windows ConPTY can be slow
        // about that).
        let mut backend = PtyBackend::new();
        let shell = if cfg!(windows) { "cmd.exe" } else { "/bin/sh" };

        let handle = backend
            .spawn(shell, TerminalSize::default())
            .await
            .expect("Failed to spawn");

        backend.kill().expect("Failed to kill");

        // Watcher polls every 200ms; allow generous slack.
        let result = tokio::time::timeout(Duration::from_secs(5), handle.exit_rx).await;
        assert!(
            matches!(result, Ok(Ok(()))),
            "exit_rx didn't fire within timeout: {result:?}"
        );
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
