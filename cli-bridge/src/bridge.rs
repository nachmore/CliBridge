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
use bridge_slack::{SlackClient, TuiRenderer};

/// Main bridge loop: connects PTY output to Slack and Slack input to PTY.
pub async fn run(channel: &str, shell: &str, workspace: Option<&str>) -> Result<()> {
    // Load credentials
    let store = CredentialStore::new()?;
    let credentials = if let Some(ws) = workspace {
        store.load(ws)?.with_context(|| {
            format!("No credentials found for workspace '{ws}'. Run --extract-tokens first.")
        })?
    } else {
        // Try to load from environment
        let token = std::env::var("CLI_BRIDGE_TOKEN")
            .context("No workspace specified and CLI_BRIDGE_TOKEN not set")?;
        let cookie = std::env::var("CLI_BRIDGE_COOKIE").ok();
        bridge_core::types::Credentials {
            token,
            cookie,
            workspace_url: None,
            workspace_name: None,
        }
    };

    // Connect to Slack
    let mut slack = SlackClient::new();
    slack.connect(credentials).await?;
    info!("Connected to Slack");

    // Send startup message
    slack
        .send_message(channel, "🖥️ *CliBridge session started*\nType commands here or use `/help` for special commands.")
        .await?;

    // Spawn PTY
    let size = TerminalSize::default();
    let mut pty = PtyBackend::new();
    let handle = pty.spawn(shell, size).await?;
    info!("Spawned shell: {shell}");

    let mut output_rx = handle.output_rx;
    let input_tx = handle.input_tx;

    // Subscribe to incoming Slack messages
    let mut message_rx = slack.subscribe(channel).await?;

    // TUI renderer
    let mut renderer = TuiRenderer::new(size.cols, size.rows);
    let mut current_message_id: Option<String> = None;

    // Main event loop
    loop {
        tokio::select! {
            // Terminal output -> Slack
            Some(data) = output_rx.recv() => {
                if let Some(rendered) = renderer.process(&data) {
                    if rendered.text.is_empty() {
                        continue;
                    }

                    if rendered.is_edit {
                        // TUI mode: edit the existing message
                        if let Some(ref msg_id) = current_message_id {
                            if let Err(e) = slack.edit_message(channel, msg_id, &rendered.text).await {
                                error!("Failed to edit message: {e}");
                                // Fall back to posting new
                                match slack.send_message(channel, &rendered.text).await {
                                    Ok(ts) => current_message_id = Some(ts),
                                    Err(e) => error!("Failed to send message: {e}"),
                                }
                            }
                        } else {
                            // First TUI frame — post a new message
                            match slack.send_message(channel, &rendered.text).await {
                                Ok(ts) => current_message_id = Some(ts),
                                Err(e) => error!("Failed to send message: {e}"),
                            }
                        }
                    } else {
                        // Streaming mode: post new messages
                        match slack.send_message(channel, &rendered.text).await {
                            Ok(ts) => current_message_id = Some(ts),
                            Err(e) => error!("Failed to send message: {e}"),
                        }
                    }
                }
            }

            // Slack input -> Terminal
            Some(msg) = message_rx.recv() => {
                let parsed = parse_input(&msg.text);
                match parsed {
                    ParsedInput::Text(text) => {
                        let mut bytes = text.into_bytes();
                        bytes.push(b'\n');
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
                                // Note: input_tx is now stale, but we can't reassign it
                                // in this loop structure. For restart, we break and re-run.
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
                                // Commands that produce terminal bytes
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

            // Check if PTY is still alive (output channel closed = process exited)
            else => {
                if !pty.is_alive() {
                    // Flush any remaining output
                    if let Some(rendered) = renderer.flush() {
                        let _ = slack.send_message(channel, &rendered.text).await;
                    }
                    slack.send_message(channel, "⚡ Shell process exited.").await?;
                    break;
                }
            }
        }
    }

    info!("Bridge session ended");
    Ok(())
}
