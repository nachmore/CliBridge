use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{error, info};

use bridge_auth::CredentialStore;
use bridge_core::commands::{
    ParsedInput, SpecialCommand, command_to_bytes, help_text, parse_input,
};
use bridge_core::messaging::MessagingClient;
use bridge_core::terminal::TerminalBackend;
use bridge_core::types::TerminalSize;
use bridge_pty::PtyBackend;
use bridge_slack::{RenderedOutput, SlackClient, TuiRenderer};

/// How often we drain the renderer and post to Slack. Slack's rate limit on
/// chat.postMessage is ~1 message/second per channel, so we tick a hair above
/// that to stay safely under it.
const RENDER_TICK: Duration = Duration::from_millis(1100);

/// How long to wait for the goodbye Slack post when the user hits Ctrl+C.
/// Best-effort: if the network is wedged we'd rather exit than hang.
const SHUTDOWN_POST_TIMEOUT: Duration = Duration::from_secs(2);

/// Main bridge loop: connects PTY output to Slack and Slack input to PTY.
pub async fn run(
    channel: &str,
    shell: &str,
    workspace: Option<&str>,
    api_url: Option<&str>,
) -> Result<()> {
    let store = CredentialStore::new()?;
    let credentials = if let Some(ws) = workspace {
        store.load(ws)?.with_context(|| {
            format!("No credentials found for workspace '{ws}'. Run --login first.")
        })?
    } else {
        // No workspace flag: take the only saved workspace if there's exactly one.
        let names = store.list_workspaces()?;
        match names.len() {
            0 => anyhow::bail!("No saved workspaces. Run `cli-bridge --login` first."),
            1 => store
                .load(&names[0])?
                .context("Saved workspace disappeared between list and load")?,
            _ => anyhow::bail!(
                "Multiple workspaces saved ({}). Pass --workspace <name>.",
                names.join(", ")
            ),
        }
    };

    let mut slack = if let Some(url) = api_url {
        SlackClient::new().with_api_base(url)
    } else {
        SlackClient::new()
    };
    slack.connect(credentials).await?;
    info!("Connected to Slack");

    slack
        .send_message(
            channel,
            "🖥️ *CliBridge session started*\nType commands here or use `--help` for special commands.",
        )
        .await?;

    let size = TerminalSize::default();
    let mut pty = PtyBackend::new();
    let handle = pty.spawn(shell, size).await?;
    info!("Spawned shell: {shell}");

    let mut output_rx = handle.output_rx;
    let input_tx = handle.input_tx;

    let mut message_rx = slack.subscribe(channel).await?;

    let mut renderer = TuiRenderer::new(size.cols, size.rows);
    let mut current_message_id: Option<String> = None;
    let mut tick = tokio::time::interval(RENDER_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut interrupted = false;

    loop {
        tokio::select! {
            // Pull terminal output and buffer it. Posting happens on the tick
            // below so a burst of bytes turns into one Slack message instead
            // of one per chunk.
            Some(data) = output_rx.recv() => {
                renderer.process(&data);
            }

            // Drain the renderer and post.
            _ = tick.tick() => {
                if let Some(rendered) = renderer.take_pending() {
                    post_or_edit(&slack, channel, rendered, &mut current_message_id).await;
                }
            }

            // Ctrl+C in the local terminal: notify Slack (best effort) and
            // exit cleanly. Without this, tokio::signal kills the process
            // immediately and the channel is left hanging on the prior message.
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl+C received, shutting down");
                interrupted = true;
                break;
            }

            Some(msg) = message_rx.recv() => {
                let parsed = parse_input(&msg.text);
                match parsed {
                    ParsedInput::Text(text) => {
                        let mut bytes = text.into_bytes();
                        // Submit the line. ConPTY (cmd.exe / PowerShell) needs CR;
                        // Unix shells accept either CR or LF, so CR alone works
                        // for everyone. \n on Windows just appends a literal
                        // newline to the line buffer without submitting it.
                        bytes.push(b'\r');
                        if input_tx.send(bytes).await.is_err() {
                            error!("PTY input channel closed");
                            break;
                        }
                    }
                    ParsedInput::Command(cmd) => {
                        match cmd {
                            SpecialCommand::Kill => {
                                pty.kill()?;
                                slack.send_message(channel, "💀 Shell process killed.").await?;
                                break;
                            }
                            SpecialCommand::Restart => {
                                pty.kill()?;
                                let new_handle = pty.spawn(shell, size).await?;
                                output_rx = new_handle.output_rx;
                                slack.send_message(channel, "🔄 Shell restarted.").await?;
                                current_message_id = None;
                            }
                            SpecialCommand::Resize(new_size) => {
                                pty.resize(new_size)?;
                                renderer.resize(new_size.cols, new_size.rows);
                                slack.send_message(
                                    channel,
                                    &format!("📐 Resized to {}x{}", new_size.cols, new_size.rows),
                                ).await?;
                            }
                            SpecialCommand::Clear => {
                                current_message_id = None;
                                slack.send_message(channel, "🧹 History cleared.").await?;
                            }
                            SpecialCommand::Help => {
                                slack.send_message(channel, &help_text()).await?;
                            }
                            _ => {
                                if let Some(bytes) = command_to_bytes(&cmd)
                                    && input_tx.send(bytes).await.is_err()
                                {
                                    error!("PTY input channel closed");
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            else => {
                if !pty.is_alive() {
                    if let Some(rendered) = renderer.take_pending() {
                        post_or_edit(&slack, channel, rendered, &mut current_message_id).await;
                    }
                    slack.send_message(channel, "⚡ Shell process exited.").await?;
                    break;
                }
            }
        }
    }

    if interrupted {
        // Try to flush any pending output and post a goodbye, but don't let a
        // wedged network keep us from exiting. Best effort — if Slack is slow
        // we still want Ctrl+C to feel like Ctrl+C.
        if let Some(rendered) = renderer.take_pending() {
            let _ = tokio::time::timeout(
                SHUTDOWN_POST_TIMEOUT,
                post_or_edit(&slack, channel, rendered, &mut current_message_id),
            )
            .await;
        }
        let _ = tokio::time::timeout(
            SHUTDOWN_POST_TIMEOUT,
            slack.send_message(
                channel,
                "👋 *CliBridge session ended* (interrupted by host)",
            ),
        )
        .await;
        // Best-effort kill so the shell process doesn't outlive us.
        let _ = pty.kill();
    }

    info!("Bridge session ended");
    Ok(())
}

/// Post the rendered chunk, or edit the current message in place for TUI frames.
/// Falls back to a fresh post if editing fails (e.g. message was deleted).
///
/// `current_message_id` is the ts of the message TUI frames edit each tick.
/// Streaming chunks always post anew and never become the edit target — that
/// keeps prior streaming output intact and avoids overwriting it with the
/// next TUI frame.
async fn post_or_edit(
    slack: &SlackClient,
    channel: &str,
    rendered: RenderedOutput,
    current_message_id: &mut Option<String>,
) {
    if rendered.text.is_empty() {
        return;
    }

    if rendered.is_edit
        && let Some(msg_id) = current_message_id.as_deref()
    {
        match slack.edit_message(channel, msg_id, &rendered.text).await {
            Ok(()) => return,
            Err(e) => {
                error!("Failed to edit message, falling back to new post: {e}");
                *current_message_id = None;
            }
        }
    }

    match slack.send_message(channel, &rendered.text).await {
        Ok(ts) => {
            if rendered.is_edit {
                *current_message_id = Some(ts);
            }
        }
        Err(e) => error!("Failed to send message: {e}"),
    }
}
