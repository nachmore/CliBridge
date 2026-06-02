//! Attach client: the mode of `cli-bridge --attach` that runs in a fresh
//! terminal window and pipes that terminal's stdin/stdout to a bridge
//! process over our protocol.
//!
//! This is what gives the user a "real" local terminal: their keystrokes
//! reach the shell unchanged (Ctrl+C kills the foreground job, not the
//! bridge), and the shell's output renders on their console with full
//! escape-sequence support, since their terminal natively interprets ANSI.

use std::io::{self, IsTerminal, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use crossterm::terminal;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::protocol::{self, Message};

/// Run the attach client until the connection closes.
pub async fn run(addr: &str, token: &str) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!(
            "Attach client requires a real terminal (got non-tty stdio). \
             Run cli-bridge --attach in an interactive terminal."
        );
    }

    info!("Connecting to bridge at {addr}");
    let sock = TcpStream::connect(addr)
        .await
        .with_context(|| format!("connect to bridge at {addr}"))?;
    sock.set_nodelay(true).ok();

    // Initial size — we capture it now and pass it in Hello so the bridge
    // can size the PTY immediately. Crossterm gives us the cell dimensions
    // of the underlying console.
    let (cols, rows) = terminal::size().unwrap_or((120, 24));

    // Take stdout out of cooked mode for the duration of the session and
    // restore on drop. This lets the bridge's PTY output drive the screen
    // directly without the OS interpreting it twice (e.g. on Windows where
    // line buffering would otherwise eat \r).
    let _raw_guard = RawModeGuard::enter()?;

    let (mut rd, wr) = sock.into_split();
    let mut wr = BufWriter::new(wr);

    protocol::write_frame(
        &mut wr,
        &Message::Hello {
            version: protocol::PROTOCOL_VERSION,
            cols,
            rows,
            token: token.to_string(),
        },
    )
    .await?;
    wr.flush().await?;

    let hello_resp = protocol::read_frame(&mut rd)
        .await
        .context("read HelloOk")?
        .context("server closed before responding to Hello")?;
    if !matches!(hello_resp, Message::HelloOk) {
        bail!("bridge rejected handshake: got {hello_resp:?}");
    }
    debug!("attach handshake OK ({cols}x{rows})");

    // Channels feeding the writer task. stdin reader and resize watcher both
    // produce frames; the writer task serializes them onto the socket.
    let (frame_tx, mut frame_rx) = mpsc::channel::<Message>(256);

    // Shared shutdown flag: any task that detects EOF or socket close flips
    // it; the others exit on their next iteration. Avoids a tangled
    // tokio::select! over heterogeneous loops.
    let stop = Arc::new(AtomicBool::new(false));

    // ---- stdin reader (blocking thread; raw mode = byte-oriented reads) ----
    let stdin_tx = frame_tx.clone();
    let stop_for_stdin = stop.clone();
    std::thread::spawn(move || {
        let stdin = io::stdin();
        let mut handle = stdin.lock();
        let mut buf = [0u8; 4096];
        while !stop_for_stdin.load(Ordering::Relaxed) {
            match handle.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if stdin_tx
                        .blocking_send(Message::Input(buf[..n].to_vec()))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(e) => {
                    debug!("stdin read error: {e}");
                    break;
                }
            }
        }
        stop_for_stdin.store(true, Ordering::Relaxed);
    });

    // ---- resize watcher ----
    // crossterm doesn't have a native async resize event source, but polling
    // size() once a second is cheap and Good Enough — users don't resize
    // their terminal at sub-second rates.
    let resize_tx = frame_tx.clone();
    let stop_for_resize = stop.clone();
    tokio::spawn(async move {
        let mut last = (cols, rows);
        while !stop_for_resize.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Ok(now) = terminal::size()
                && now != last
            {
                last = now;
                if resize_tx
                    .send(Message::Resize {
                        cols: now.0,
                        rows: now.1,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    });

    // ---- writer: drains frame_rx onto the socket ----
    let stop_for_writer = stop.clone();
    let writer_handle = tokio::spawn(async move {
        while let Some(msg) = frame_rx.recv().await {
            if stop_for_writer.load(Ordering::Relaxed) {
                break;
            }
            if protocol::write_frame(&mut wr, &msg).await.is_err() {
                break;
            }
            if wr.flush().await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
        stop_for_writer.store(true, Ordering::Relaxed);
    });

    // ---- reader: pulls Output frames and writes them to stdout ----
    // We write to stdout from a blocking section because stdout writes are
    // tiny syscalls and going async-via-spawn_blocking per frame adds latency.
    let stop_for_reader = stop.clone();
    let mut stdout = io::stdout().lock();
    while !stop_for_reader.load(Ordering::Relaxed) {
        let frame = match protocol::read_frame(&mut rd).await {
            Ok(Some(m)) => m,
            Ok(None) => {
                debug!("bridge closed connection");
                break;
            }
            Err(e) => {
                warn!("attach socket read error: {e}");
                break;
            }
        };
        match frame {
            Message::Output(bytes) => {
                if stdout.write_all(&bytes).is_err() {
                    break;
                }
                if stdout.flush().is_err() {
                    break;
                }
            }
            Message::Goodbye => {
                debug!("bridge sent Goodbye");
                break;
            }
            other => {
                debug!("attach client ignoring unexpected frame: {other:?}");
            }
        }
    }
    stop.store(true, Ordering::Relaxed);
    drop(frame_tx);
    // Don't await the writer — the stdin reader thread still holds a clone
    // of frame_tx (it's parked in a blocking read() until the next keystroke,
    // so we can't make it drop the clone synchronously). With that clone
    // alive, frame_rx.recv() in the writer will never return None and the
    // await would hang forever, keeping the process alive after Ctrl+C on
    // the bridge until the user typed in the local window.
    //
    // Aborting is fine: the writer task only flushes pending Input/Resize
    // frames, and at this point either the bridge is gone (so the socket
    // would error anyway) or we asked it to disconnect us (Goodbye).
    writer_handle.abort();
    // The detached stdin thread will be torn down when the process exits.

    Ok(())
}

/// RAII guard that puts the terminal in raw mode for the lifetime of the
/// attach session. Restores cooked mode on drop, even if the program panics
/// — without this, a crash would leave the user's terminal unusable until
/// they typed `reset` blind.
struct RawModeGuard;

impl RawModeGuard {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode().context("enable terminal raw mode")?;
        // On Windows, raw mode alone doesn't make special keys (Esc, Ctrl,
        // Alt, PgUp/PgDn, arrows) arrive on stdin — they're delivered as
        // console KEY_EVENT records that a plain read() never sees. Enabling
        // virtual-terminal input makes the console encode them as VT escape
        // sequences, which then flow through our byte-oriented read loop
        // unchanged, exactly like a Unix terminal. No-op on other platforms.
        platform::enable_vt_input().context("enable virtual terminal input")?;
        Ok(Self)
    }
}

#[cfg(windows)]
mod platform {
    use anyhow::{Result, bail};
    use winapi::um::consoleapi::{GetConsoleMode, SetConsoleMode};
    use winapi::um::processenv::GetStdHandle;
    use winapi::um::winbase::STD_INPUT_HANDLE;
    use winapi::um::wincon::ENABLE_VIRTUAL_TERMINAL_INPUT;

    /// Add ENABLE_VIRTUAL_TERMINAL_INPUT to the console input mode so special
    /// keys are translated to VT escape sequences on stdin. We deliberately
    /// don't restore the previous mode: the attach client owns its terminal
    /// window for the whole process lifetime, and the window is torn down when
    /// the session ends, so there's nothing to restore into.
    pub(super) fn enable_vt_input() -> Result<()> {
        // SAFETY: standard Win32 console calls. GetStdHandle returns a handle
        // owned by the process (not to be closed); we only read and re-set the
        // console mode bits on it.
        unsafe {
            let handle = GetStdHandle(STD_INPUT_HANDLE);
            if handle.is_null() || handle == winapi::um::handleapi::INVALID_HANDLE_VALUE {
                bail!("GetStdHandle(STD_INPUT_HANDLE) returned an invalid handle");
            }
            let mut mode: u32 = 0;
            if GetConsoleMode(handle, &mut mode) == 0 {
                bail!("GetConsoleMode failed: {}", std::io::Error::last_os_error());
            }
            if SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_INPUT) == 0 {
                bail!("SetConsoleMode failed: {}", std::io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod platform {
    use anyhow::Result;

    /// On Unix terminals special keys already arrive as VT escape sequences,
    /// so there's nothing to toggle.
    pub(super) fn enable_vt_input() -> Result<()> {
        Ok(())
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if let Err(e) = terminal::disable_raw_mode() {
            // Print to stderr — at this point our raw mode is questionable
            // anyway, and leaving the user without feedback is worse.
            eprintln!("warning: failed to restore terminal mode: {e}");
        }
    }
}
