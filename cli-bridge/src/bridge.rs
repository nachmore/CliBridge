use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tracing::{error, info};

use bridge_auth::CredentialStore;
use bridge_core::commands::{
    ParsedInput, SpecialCommand, command_to_bytes, help_text, parse_input,
};
use bridge_core::messaging::MessagingClient;
use bridge_core::terminal::TerminalBackend;
use bridge_core::types::{IncomingMessage, TerminalSize};
use bridge_pty::PtyBackend;
use bridge_slack::{RenderedOutput, SlackClient, TuiRenderer};

use crate::attach::{AttachEvent, AttachServer, open_attach_terminal, print_manual_attach_hint};

/// How often we drain the renderer and post to Slack. Slack's rate limit on
/// chat.postMessage is ~1 message/second per channel, so we tick a hair above
/// that to stay safely under it.
const RENDER_TICK: Duration = Duration::from_millis(1100);

/// How long to wait for the goodbye Slack post when the user hits Ctrl+C.
/// Best-effort: if the network is wedged we'd rather exit than hang.
const SHUTDOWN_POST_TIMEOUT: Duration = Duration::from_secs(2);

/// Reason a shell session ended. The outer run loop uses this to decide
/// whether to spawn a new session, ask the user, or quit entirely.
enum SessionEnd {
    /// Shell process exited on its own (user typed `exit`, or the process
    /// crashed). The outer loop posts a "shell exited; --new to restart"
    /// message and waits for the user.
    ShellExited,
    /// User asked for a new shell via `--new` / `--restart` from Slack.
    /// Outer loop spawns immediately without prompting.
    UserRequestedNew,
    /// User asked to kill via `--kill`. Outer loop exits cleanly.
    UserKilled,
    /// Local Ctrl+C on the bridge process. Outer loop exits cleanly.
    Interrupted,
}

/// Top-level entrypoint. Manages session lifecycle: connects to Slack once,
/// then spawns shell sessions as needed and supervises them.
pub async fn run(
    channel: &str,
    shell: &str,
    workspace: Option<&str>,
    api_url: Option<&str>,
    size: TerminalSize,
    local: bool,
) -> Result<()> {
    let store = CredentialStore::new()?;
    let credentials = if let Some(ws) = workspace {
        store.load(ws)?.with_context(|| {
            format!("No credentials found for workspace '{ws}'. Run --login first.")
        })?
    } else {
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

    // Subscribe to Slack messages once and reuse the receiver across sessions.
    // Resubscribing is expensive (full channel re-poll); keeping it open means
    // `--new` from Slack always works even between sessions.
    let mut message_rx = slack.subscribe(channel).await?;

    loop {
        let outcome = run_session(&slack, &mut message_rx, channel, shell, size, local).await?;
        match outcome {
            SessionEnd::ShellExited => {
                // Wait for `--new` (Slack) or Ctrl+C (local). Don't auto-spawn
                // — user might want to look at the final output.
                slack
                    .send_message(
                        channel,
                        "⚡ *Shell exited.* Send `--new` to start a new shell, or Ctrl+C in the bridge window to quit.",
                    )
                    .await?;
                if !await_new_or_quit(&mut message_rx, &slack, channel).await? {
                    break;
                }
                slack
                    .send_message(channel, "🔄 *Starting new shell…*")
                    .await?;
            }
            SessionEnd::UserRequestedNew => {
                slack
                    .send_message(channel, "🔄 *Restarting shell at user request…*")
                    .await?;
            }
            SessionEnd::UserKilled => {
                slack.send_message(channel, "💀 *Shell killed.*").await?;
                break;
            }
            SessionEnd::Interrupted => {
                let _ = tokio::time::timeout(
                    SHUTDOWN_POST_TIMEOUT,
                    slack.send_message(
                        channel,
                        "👋 *CliBridge session ended* (interrupted by host)",
                    ),
                )
                .await;
                break;
            }
        }
    }

    info!("Bridge run loop ended");
    Ok(())
}

/// Run a single shell session: spawn PTY + (optional) attach server, pump I/O
/// between PTY, Slack, and attach clients until the session ends.
async fn run_session(
    slack: &SlackClient,
    message_rx: &mut mpsc::Receiver<IncomingMessage>,
    channel: &str,
    shell: &str,
    size: TerminalSize,
    local: bool,
) -> Result<SessionEnd> {
    // Start a fresh attach server per session. Old attach clients (from a
    // previous session) have already disconnected because their server was
    // dropped; a brand-new bind avoids any state leakage between sessions.
    let mut attach: Option<AttachServer> = if local {
        match AttachServer::start().await {
            Ok(s) => {
                let addr = s.addr.to_string();
                let token = s.token.clone();
                match open_attach_terminal(&addr, &token) {
                    Ok(()) => {}
                    Err(e) => {
                        info!("Auto-spawn of attach terminal failed: {e}");
                        print_manual_attach_hint(&addr, &token);
                    }
                }
                Some(s)
            }
            Err(e) => {
                info!("Could not start attach server, continuing Slack-only: {e}");
                None
            }
        }
    } else {
        None
    };

    let mut pty = PtyBackend::new();
    let handle = pty.spawn(shell, size).await?;
    info!("Spawned shell: {shell} ({}x{})", size.cols, size.rows);

    let mut output_rx = handle.output_rx;
    let input_tx = handle.input_tx;

    let mut renderer = TuiRenderer::new(size.cols, size.rows);
    let mut current_message_id: Option<String> = None;
    let mut tick = tokio::time::interval(RENDER_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Split the attach server so we can both forward output and drain events.
    let attach_output = attach.as_ref().map(|a| a.output_tx.clone());
    let mut attach_events = attach
        .as_mut()
        .map(|a| std::mem::replace(&mut a.events, tokio::sync::mpsc::channel(1).1));

    let outcome = loop {
        let attach_event_recv = async {
            match attach_events.as_mut() {
                Some(rx) => rx.recv().await,
                None => std::future::pending::<Option<AttachEvent>>().await,
            }
        };

        tokio::select! {
            // PTY output: forward to renderer + attach clients. None means
            // the shell process has exited — we drop the attach server (so
            // attached terminal windows close), flush trailing renderer
            // output to Slack, and end the session.
            data = output_rx.recv() => {
                match data {
                    Some(bytes) => {
                        renderer.process(&bytes);
                        if let Some(tx) = attach_output.as_ref() {
                            let _ = tx.send(Arc::new(bytes));
                        }
                    }
                    None => {
                        info!("Shell output channel closed (shell exited)");
                        // Dropping the AttachServer also drops its output_tx
                        // sender; attach_output is the only other clone, and
                        // it goes out of scope when this loop returns. Once
                        // both are gone, broadcast::Receiver returns Closed
                        // and each client task writes Goodbye + disconnects.
                        let _ = attach.take();

                        if let Some(rendered) = renderer.take_pending() {
                            post_or_edit(slack, channel, rendered, &mut current_message_id).await;
                        }
                        break SessionEnd::ShellExited;
                    }
                }
            }

            // Input or resize from an attach client.
            Some(evt) = attach_event_recv => {
                match evt {
                    AttachEvent::Input(bytes) => {
                        if input_tx.send(bytes).await.is_err() {
                            error!("PTY input channel closed");
                            // Will be picked up by the output_rx None branch.
                        }
                    }
                    AttachEvent::Resize(new_size) => {
                        if let Err(e) = pty.resize(new_size) {
                            error!("Failed to resize PTY: {e}");
                        }
                        renderer.resize(new_size.cols, new_size.rows);
                    }
                }
            }

            // Drain the renderer and post.
            _ = tick.tick() => {
                if let Some(rendered) = renderer.take_pending() {
                    post_or_edit(slack, channel, rendered, &mut current_message_id).await;
                }
            }

            // Local Ctrl+C: end the whole bridge run (not just this session).
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl+C received, shutting down");
                if let Some(rendered) = renderer.take_pending() {
                    let _ = tokio::time::timeout(
                        SHUTDOWN_POST_TIMEOUT,
                        post_or_edit(slack, channel, rendered, &mut current_message_id),
                    ).await;
                }
                let _ = pty.kill();
                break SessionEnd::Interrupted;
            }

            msg = message_rx.recv() => {
                let Some(msg) = msg else {
                    // Slack subscription died — something's wrong upstream.
                    // Treat as interrupt; the outer loop will quit.
                    error!("Slack subscription channel closed");
                    let _ = pty.kill();
                    break SessionEnd::Interrupted;
                };
                match handle_slack_message(
                    &msg,
                    &input_tx,
                    &mut pty,
                    &mut renderer,
                    slack,
                    channel,
                    &mut current_message_id,
                ).await {
                    SlackOutcome::Continue => {}
                    SlackOutcome::Kill => {
                        let _ = pty.kill();
                        break SessionEnd::UserKilled;
                    }
                    SlackOutcome::RestartOrNew => {
                        let _ = pty.kill();
                        break SessionEnd::UserRequestedNew;
                    }
                }
            }
        }
    };

    Ok(outcome)
}

/// Wait, between sessions, for either a `--new` from Slack (returns `true`)
/// or local Ctrl+C (returns `false`). All other Slack input during this
/// window is ignored — there's no shell to receive it. Special commands
/// other than `--new` get a friendly nudge.
async fn await_new_or_quit(
    message_rx: &mut mpsc::Receiver<IncomingMessage>,
    slack: &SlackClient,
    channel: &str,
) -> Result<bool> {
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl+C while idle, exiting");
                return Ok(false);
            }
            msg = message_rx.recv() => {
                let Some(msg) = msg else {
                    return Ok(false);
                };
                match parse_input(&msg.text) {
                    ParsedInput::Command(SpecialCommand::Restart) => return Ok(true),
                    ParsedInput::Command(SpecialCommand::Help) => {
                        slack.send_message(channel, &help_text()).await?;
                    }
                    ParsedInput::Command(SpecialCommand::Kill) => {
                        slack.send_message(channel, "💀 No shell to kill — already exited.").await?;
                        return Ok(false);
                    }
                    _ => {
                        slack
                            .send_message(
                                channel,
                                "ℹ️ Shell has exited. Send `--new` to start a new shell.",
                            )
                            .await?;
                    }
                }
            }
        }
    }
}

/// What to do after handling one inbound Slack message in the session loop.
enum SlackOutcome {
    Continue,
    Kill,
    RestartOrNew,
}

async fn handle_slack_message(
    msg: &IncomingMessage,
    input_tx: &mpsc::Sender<Vec<u8>>,
    pty: &mut PtyBackend,
    renderer: &mut TuiRenderer,
    slack: &SlackClient,
    channel: &str,
    current_message_id: &mut Option<String>,
) -> SlackOutcome {
    let parsed = parse_input(&msg.text);
    match parsed {
        ParsedInput::Text(text) => {
            let mut bytes = text.into_bytes();
            // Submit the line. ConPTY needs CR; Unix shells accept it too.
            bytes.push(b'\r');
            if input_tx.send(bytes).await.is_err() {
                error!("PTY input channel closed");
            }
            SlackOutcome::Continue
        }
        ParsedInput::Command(cmd) => match cmd {
            SpecialCommand::Kill => SlackOutcome::Kill,
            SpecialCommand::Restart => SlackOutcome::RestartOrNew,
            SpecialCommand::Resize(new_size) => {
                if let Err(e) = pty.resize(new_size) {
                    error!("Failed to resize PTY: {e}");
                }
                renderer.resize(new_size.cols, new_size.rows);
                let _ = slack
                    .send_message(
                        channel,
                        &format!("📐 Resized to {}x{}", new_size.cols, new_size.rows),
                    )
                    .await;
                SlackOutcome::Continue
            }
            SpecialCommand::Clear => {
                *current_message_id = None;
                let _ = slack.send_message(channel, "🧹 History cleared.").await;
                SlackOutcome::Continue
            }
            SpecialCommand::Help => {
                let _ = slack.send_message(channel, &help_text()).await;
                SlackOutcome::Continue
            }
            other => {
                if let Some(bytes) = command_to_bytes(&other)
                    && input_tx.send(bytes).await.is_err()
                {
                    error!("PTY input channel closed");
                }
                SlackOutcome::Continue
            }
        },
    }
}

/// Post the rendered chunk, or edit the current message in place for TUI frames.
/// Falls back to a fresh post if editing fails.
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
