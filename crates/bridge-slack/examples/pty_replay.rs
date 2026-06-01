//! Replay a captured PTY byte stream through the renderer and print the
//! resulting frame. Use this to post-mortem rendering bugs offline:
//!
//! ```text
//! cli-bridge --pty-log pty.bin --workspace ws --channel C123
//! # … reproduce the artifact, then Ctrl+C the bridge.
//! RUST_LOG=bridge_slack=trace cargo run -p bridge-slack \
//!     --example pty_replay -- pty.bin 120 24
//! ```
//!
//! Output: the final virtual screen state, plus (with trace logging) every
//! CSI dispatch with cursor before/after — the breadcrumb trail to find
//! where the layout went wrong.
//!
//! Args:
//!   pty_replay <log-path> [cols] [rows] [chunk-size]
//!     cols       defaults to 120
//!     rows       defaults to 24
//!     chunk-size defaults to 4096 (matches the PTY reader). Lower values
//!                stress the chunk-boundary carry-buffer logic.

use std::env;
use std::fs;

use bridge_slack::TuiRenderer;
use tracing_subscriber::EnvFilter;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let mut args = env::args().skip(1);
    let path = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: pty_replay <log-path> [cols] [rows] [chunk]"))?;
    let cols: u16 = args.next().as_deref().unwrap_or("120").parse()?;
    let rows: u16 = args.next().as_deref().unwrap_or("24").parse()?;
    let chunk: usize = args.next().as_deref().unwrap_or("4096").parse()?;

    let bytes = fs::read(&path)?;
    eprintln!("loaded {} bytes from {path}", bytes.len());

    let mut renderer = TuiRenderer::new(cols, rows);
    for slice in bytes.chunks(chunk) {
        renderer.process(slice);
    }
    let out = renderer
        .take_pending()
        .map(|r| r.text)
        .unwrap_or_else(|| "(no output)".to_string());
    println!("{out}");
    Ok(())
}
