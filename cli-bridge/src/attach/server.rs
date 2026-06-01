//! Attach server: listens on a loopback TCP port, accepts attach clients, and
//! pipes I/O between them and the rest of the bridge.
//!
//! Concurrency model:
//! - One `tokio::sync::broadcast` channel for **PTY-output → all clients**.
//!   Each client task owns one `Receiver`. Slow clients get lagged out
//!   (they'll see a `RecvError::Lagged` and we drop them) rather than
//!   stalling the PTY reader, which keeps the bridge responsive.
//! - One `tokio::sync::mpsc` channel for **clients → bridge** carrying both
//!   input bytes and resize events. Multiple clients can share it; the bridge
//!   drains it from its main `select!` loop.
//!
//! Auth:
//! - Each session generates a fresh 32-hex-char token. Clients send it in
//!   their Hello frame; mismatch → kick before we hand them the broadcast.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};
use tokio::time::timeout;
use tracing::{debug, info, warn};

use bridge_core::types::TerminalSize;

use super::protocol::{self, Message};

/// How long a client has to send its Hello frame before we drop the connection.
/// Real clients send it in milliseconds; if a port-scanner connects, we'd
/// rather time out than tie up resources.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

/// Broadcast buffer size. Each slot holds one PTY-output frame (typically a
/// few KiB). 1024 slots ≈ a few MiB of high-water memory if a client stalls,
/// which is fine — and once it's full, the slow client gets lagged out
/// rather than slowing the producer.
const BROADCAST_CAPACITY: usize = 1024;

/// Events the server emits up to the bridge's main loop. The bridge drains a
/// receiver on these and applies them to the PTY / renderer.
#[derive(Debug, Clone)]
pub enum AttachEvent {
    /// Bytes typed by an attach client; should be written to the PTY input.
    Input(Vec<u8>),
    /// An attach client's terminal was resized; the bridge should resize the
    /// PTY and the renderer to match.
    Resize(TerminalSize),
}

/// Handle the bridge holds onto. Wraps the broadcast sender (so we can fan
/// PTY output out to all clients) and the mpsc receiver for events coming
/// back the other way.
pub struct AttachServer {
    /// Local address the server is bound to (e.g. "127.0.0.1:54321"). The
    /// bridge passes this to the spawned attach process.
    pub addr: SocketAddr,
    /// Per-session shared secret. Clients must echo it in Hello.
    pub token: String,
    /// PTY output → all clients. The bridge sends every output chunk here.
    pub output_tx: broadcast::Sender<Arc<Vec<u8>>>,
    /// Events from clients → bridge.
    pub events: mpsc::Receiver<AttachEvent>,
}

impl AttachServer {
    /// Bind to `127.0.0.1:0` (kernel-allocated port), generate a session
    /// token, and spawn the accept loop. Returns a handle the bridge uses.
    pub async fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("bind 127.0.0.1:0 for attach server")?;
        let addr = listener
            .local_addr()
            .context("read attach server local addr")?;
        let token = generate_token();

        // Cap clients at... a lot, but with a sender we can clone freely.
        // Output broadcast is 1:N where N = current attach clients.
        let (output_tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel(256);

        let token_for_loop = token.clone();
        let output_for_loop = output_tx.clone();
        tokio::spawn(async move {
            accept_loop(listener, token_for_loop, output_for_loop, event_tx).await;
        });

        info!("Attach server listening on {addr}");
        Ok(Self {
            addr,
            token,
            output_tx,
            events: event_rx,
        })
    }
}

async fn accept_loop(
    listener: TcpListener,
    token: String,
    output_tx: broadcast::Sender<Arc<Vec<u8>>>,
    event_tx: mpsc::Sender<AttachEvent>,
) {
    loop {
        let (sock, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                warn!("attach accept error: {e}");
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
        };
        debug!("attach client connected: {peer}");

        // Disable Nagle: we want input keystrokes to land on the PTY without
        // up-to-200ms latency added by coalescing.
        if let Err(e) = sock.set_nodelay(true) {
            debug!("could not set TCP_NODELAY on attach client: {e}");
        }

        let token = token.clone();
        let output_tx = output_tx.clone();
        let event_tx = event_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(sock, &token, output_tx, event_tx).await {
                debug!("attach client {peer} ended: {e}");
            }
        });
    }
}

async fn handle_client(
    sock: TcpStream,
    expected_token: &str,
    output_tx: broadcast::Sender<Arc<Vec<u8>>>,
    event_tx: mpsc::Sender<AttachEvent>,
) -> Result<()> {
    let (mut rd, wr) = sock.into_split();
    // Buffer writes so we don't make a syscall per Output frame; the writer
    // task explicitly flushes after each frame to keep latency low.
    let mut wr = BufWriter::new(wr);

    // ---- Handshake: expect Hello within HELLO_TIMEOUT ----
    let hello = match timeout(HELLO_TIMEOUT, protocol::read_frame(&mut rd)).await {
        Ok(Ok(Some(m))) => m,
        Ok(Ok(None)) => anyhow::bail!("client closed before sending Hello"),
        Ok(Err(e)) => anyhow::bail!("read Hello: {e}"),
        Err(_) => anyhow::bail!("timed out waiting for Hello"),
    };

    let (cols, rows) = match hello {
        Message::Hello {
            version,
            cols,
            rows,
            token,
        } => {
            if version != protocol::PROTOCOL_VERSION {
                anyhow::bail!(
                    "client protocol version {version} != server {}",
                    protocol::PROTOCOL_VERSION
                );
            }
            if !constant_time_eq(token.as_bytes(), expected_token.as_bytes()) {
                anyhow::bail!("invalid attach token");
            }
            (cols, rows)
        }
        other => anyhow::bail!("expected Hello, got {other:?}"),
    };

    protocol::write_frame(&mut wr, &Message::HelloOk).await?;
    wr.flush().await?;

    // Tell the bridge about the client's initial size so the PTY matches.
    let _ = event_tx
        .send(AttachEvent::Resize(TerminalSize { cols, rows }))
        .await;

    // ---- Reader task: client → bridge ----
    let event_tx_for_reader = event_tx.clone();
    let reader_handle = tokio::spawn(async move {
        loop {
            let frame = match protocol::read_frame(&mut rd).await {
                Ok(Some(m)) => m,
                Ok(None) => break, // clean EOF
                Err(e) => {
                    debug!("attach reader error: {e}");
                    break;
                }
            };
            match frame {
                Message::Input(bytes) => {
                    if event_tx_for_reader
                        .send(AttachEvent::Input(bytes))
                        .await
                        .is_err()
                    {
                        break; // bridge gone
                    }
                }
                Message::Resize { cols, rows } => {
                    let _ = event_tx_for_reader
                        .send(AttachEvent::Resize(TerminalSize { cols, rows }))
                        .await;
                }
                Message::Goodbye => break,
                _ => {
                    // Server-bound message types from a client are ignored;
                    // we don't disconnect over them so a future protocol
                    // extension stays backward-compatible.
                    debug!("ignoring unexpected client frame: {frame:?}");
                }
            }
        }
    });

    // ---- Writer task: bridge → client ----
    let mut output_rx = output_tx.subscribe();
    loop {
        match output_rx.recv().await {
            Ok(bytes) => {
                if let Err(e) =
                    protocol::write_frame(&mut wr, &Message::Output((*bytes).clone())).await
                {
                    debug!("attach write error: {e}");
                    break;
                }
                if wr.flush().await.is_err() {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                // Slow client. Drop them rather than letting the producer
                // back up — Slack rendering is more important than keeping
                // a wedged attach client alive.
                warn!("attach client lagged by {n} frames; disconnecting");
                let _ = protocol::write_frame(&mut wr, &Message::Goodbye).await;
                let _ = wr.flush().await;
                break;
            }
            Err(broadcast::error::RecvError::Closed) => {
                // Server is shutting down (shell exited or bridge interrupted).
                // Send an explicit Goodbye so the client's reader loop breaks
                // immediately rather than waiting for the TCP close to
                // propagate — without this, the local terminal window can sit
                // there for a beat after Ctrl+C on the bridge.
                let _ = protocol::write_frame(&mut wr, &Message::Goodbye).await;
                let _ = wr.flush().await;
                break;
            }
        }
    }

    reader_handle.abort();
    Ok(())
}

/// Generate a 32-hex-char token from an OS RNG.
fn generate_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    let mut out = String::with_capacity(protocol::TOKEN_LEN);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Constant-time byte comparison so a hostile peer can't mount a timing
/// side-channel against the token check. Loopback only, so this is belt-and-
/// suspenders, but it's free.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_hex_and_correct_length() {
        let t = generate_token();
        assert_eq!(t.len(), protocol::TOKEN_LEN);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn token_is_random() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b);
    }

    #[test]
    fn constant_time_eq_basic() {
        assert!(constant_time_eq(b"hello", b"hello"));
        assert!(!constant_time_eq(b"hello", b"world"));
        assert!(!constant_time_eq(b"hello", b"hellox"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    #[tokio::test]
    async fn server_handshake_and_io() {
        let server = AttachServer::start().await.unwrap();
        let addr = server.addr;
        let token = server.token.clone();
        let output_tx = server.output_tx.clone();
        let mut events = server.events;

        // Client connects, says Hello, sends some Input, expects Output back.
        let client = tokio::spawn(async move {
            let mut sock = TcpStream::connect(addr).await.unwrap();
            let (mut rd, mut wr) = sock.split();
            protocol::write_frame(
                &mut wr,
                &Message::Hello {
                    version: protocol::PROTOCOL_VERSION,
                    cols: 100,
                    rows: 30,
                    token,
                },
            )
            .await
            .unwrap();

            let resp = protocol::read_frame(&mut rd).await.unwrap().unwrap();
            assert_eq!(resp, Message::HelloOk);

            protocol::write_frame(&mut wr, &Message::Input(b"hi".to_vec()))
                .await
                .unwrap();

            // Read whatever the server sends back.
            let out = protocol::read_frame(&mut rd).await.unwrap().unwrap();
            assert_eq!(out, Message::Output(b"server-greets".to_vec()));
        });

        // Bridge side: expect a Resize (initial size from Hello), then Input.
        let resize_evt = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            resize_evt,
            AttachEvent::Resize(TerminalSize {
                cols: 100,
                rows: 30
            })
        ));

        let input_evt = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        match input_evt {
            AttachEvent::Input(b) => assert_eq!(b, b"hi"),
            other => panic!("expected Input, got {other:?}"),
        }

        // Bridge sends some output back.
        output_tx.send(Arc::new(b"server-greets".to_vec())).unwrap();

        client.await.unwrap();
    }

    #[tokio::test]
    async fn server_rejects_bad_token() {
        let server = AttachServer::start().await.unwrap();
        let addr = server.addr;

        let mut sock = TcpStream::connect(addr).await.unwrap();
        let (mut rd, mut wr) = sock.split();
        protocol::write_frame(
            &mut wr,
            &Message::Hello {
                version: protocol::PROTOCOL_VERSION,
                cols: 80,
                rows: 24,
                token: "00000000000000000000000000000000".to_string(),
            },
        )
        .await
        .unwrap();
        // Server should drop us without sending HelloOk.
        let frame =
            tokio::time::timeout(Duration::from_secs(2), protocol::read_frame(&mut rd)).await;
        // Either Ok(None) (clean close) or Ok(Err)/timeout → all acceptable
        // outcomes. The key contract: we did NOT receive HelloOk.
        if let Ok(Ok(Some(Message::HelloOk))) = frame {
            panic!("server accepted bad token");
        }
    }
}
