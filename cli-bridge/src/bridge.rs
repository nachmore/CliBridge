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
#[allow(clippy::too_many_arguments)]
pub async fn run(
    channel: &str,
    shell: &str,
    workspace: Option<&str>,
    api_url: Option<&str>,
    size: TerminalSize,
    local: bool,
    anchor_refresh: u32,
    name: String,
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

    // Wrap the name in Arc<Mutex<_>> so the session task can mutate it on
    // `--name <text>` from Slack and the outer loop sees the new value when
    // posting between-session banners. Locks are held for microseconds at a
    // time around format!() calls; not a perf concern.
    let name = std::sync::Arc::new(std::sync::Mutex::new(name));

    slack
        .send_message(
            channel,
            &format!(
                "🖥️ *{} session started*\nType commands here or use `--help` for special commands.",
                read_name(&name)
            ),
        )
        .await?;

    // Subscribe to Slack messages once and reuse the receiver across sessions.
    // Resubscribing is expensive (full channel re-poll); keeping it open means
    // `--new` from Slack always works even between sessions.
    let mut message_rx = slack.subscribe(channel).await?;

    loop {
        let outcome = run_session(
            &slack,
            &mut message_rx,
            channel,
            shell,
            size,
            local,
            anchor_refresh,
            name.clone(),
        )
        .await?;
        match outcome {
            SessionEnd::ShellExited => {
                // Wait for `--new` (Slack) or Ctrl+C (local). Don't auto-spawn
                // — user might want to look at the final output.
                slack
                    .send_message(
                        channel,
                        &format!(
                            "⚡ *{} shell exited.* Send `--new` to start a new shell, or Ctrl+C in the bridge window to quit.",
                            read_name(&name)
                        ),
                    )
                    .await?;
                if !await_new_or_quit(&mut message_rx, &slack, channel, name.clone()).await? {
                    break;
                }
                slack
                    .send_message(
                        channel,
                        &format!("🔄 *Starting new shell for {}…*", read_name(&name)),
                    )
                    .await?;
            }
            SessionEnd::UserRequestedNew => {
                slack
                    .send_message(
                        channel,
                        &format!("🔄 *Restarting {} at user request…*", read_name(&name)),
                    )
                    .await?;
            }
            SessionEnd::UserKilled => {
                slack
                    .send_message(channel, &format!("💀 *{} killed.*", read_name(&name)))
                    .await?;
                break;
            }
            SessionEnd::Interrupted => {
                let _ = tokio::time::timeout(
                    SHUTDOWN_POST_TIMEOUT,
                    slack.send_message(
                        channel,
                        &format!("👋 *{} ended* (interrupted by host)", read_name(&name)),
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

/// Read the current session name. Held briefly under the mutex; the lock
/// is uncontended in steady state (only the session loop writes, only the
/// banner-posting paths read).
fn read_name(name: &std::sync::Arc<std::sync::Mutex<String>>) -> String {
    name.lock()
        .map(|g| g.clone())
        .unwrap_or_else(|_| "CliBridge".to_string())
}

/// Build the `\x1b]0;<title>\x07` OSC 0 frame ("set window title"). Both
/// connected terminals and our own renderer accept this — the renderer's
/// strip_ansi drops OSC bodies, so the same bytes work for Slack and attach.
///
/// Sanitizes BEL and ESC out of the title since they'd terminate the
/// sequence prematurely; control chars get stripped wholesale.
fn build_title_frame(title: &str) -> Arc<Vec<u8>> {
    let cleaned: String = title
        .chars()
        .filter(|c| !c.is_control() && *c != '\u{1b}')
        .collect();
    let mut bytes = Vec::with_capacity(cleaned.len() + 4);
    bytes.extend_from_slice(b"\x1b]0;");
    bytes.extend_from_slice(cleaned.as_bytes());
    bytes.push(0x07);
    Arc::new(bytes)
}

/// Run a single shell session: spawn PTY + (optional) attach server, pump I/O
/// between PTY, Slack, and attach clients until the session ends.
#[allow(clippy::too_many_arguments)]
async fn run_session(
    slack: &SlackClient,
    message_rx: &mut mpsc::Receiver<IncomingMessage>,
    channel: &str,
    shell: &str,
    size: TerminalSize,
    local: bool,
    anchor_refresh: u32,
    name: std::sync::Arc<std::sync::Mutex<String>>,
) -> Result<SessionEnd> {
    // Start a fresh attach server per session. Old attach clients (from a
    // previous session) have already disconnected because their server was
    // dropped; a brand-new bind avoids any state leakage between sessions.
    let mut attach: Option<AttachServer> = if local {
        match AttachServer::start().await {
            Ok(s) => {
                let addr = s.addr.to_string();
                let token = s.token.clone();
                let session_title = read_name(&name);
                match open_attach_terminal(&addr, &token, &session_title) {
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

    // Push the current session name as the attach window title. The attach
    // server latches the frame so any client that connects after this point
    // (handshake races the spawn) still gets the right title.
    if let Some(s) = attach.as_ref() {
        s.set_title(build_title_frame(&read_name(&name)));
    }

    let mut output_rx = handle.output_rx;
    let input_tx = handle.input_tx;

    let mut renderer = TuiRenderer::new(size.cols, size.rows);
    let mut current_message_id: Option<String> = None;
    let mut tick = tokio::time::interval(RENDER_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Counter for the live-message re-anchor feature: every N inbound Slack
    // messages, drop current_message_id so the next TUI frame posts a NEW
    // message instead of editing the old one. This keeps the live frame
    // visible near the bottom of the channel as the user types. 0 disables.
    let mut messages_since_anchor: u32 = 0;

    // Split the attach server so we can both forward output and drain events.
    // Both `attach` and `attach_output` need to be droppable so that all
    // broadcast senders can be released — only when *no senders* remain do
    // subscribers see RecvError::Closed and write their final Goodbye to the
    // attach client.
    let mut attach_output = attach.as_ref().map(|a| a.output_tx.clone());
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
                        // Drop both broadcast senders so client tasks see
                        // RecvError::Closed and write Goodbye immediately —
                        // before we block on the trailing Slack post.
                        let _ = attach.take();
                        let _ = attach_output.take();

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
            // Tear down in this order so the local terminal window closes
            // promptly rather than waiting for the Slack post to finish:
            //   1. Drop both broadcast senders. Client writer tasks see
            //      RecvError::Closed, send Goodbye, and exit; client
            //      processes return and the spawned terminal windows close.
            //   2. Kill the PTY child so the shell doesn't outlive us.
            //   3. Best-effort post any pending renderer output to Slack.
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl+C received, shutting down");
                let _ = attach.take();
                let _ = attach_output.take();
                let _ = pty.kill();
                if let Some(rendered) = renderer.take_pending() {
                    let _ = tokio::time::timeout(
                        SHUTDOWN_POST_TIMEOUT,
                        post_or_edit(slack, channel, rendered, &mut current_message_id),
                    ).await;
                }
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
                let outcome = handle_slack_message(
                    &msg,
                    &input_tx,
                    &mut pty,
                    &mut renderer,
                    slack,
                    channel,
                    &mut current_message_id,
                    &name,
                    attach.as_ref(),
                ).await;

                // Re-anchor: every Nth inbound message, force the next TUI
                // frame to post a NEW message instead of editing the old one.
                // This keeps the live frame near the bottom of the channel
                // so the user doesn't have to scroll back up to see it.
                // The `--clear` command (handled inside handle_slack_message)
                // already nulls current_message_id, so we treat that path as
                // an implicit anchor reset by checking is_some() first.
                if anchor_refresh > 0 && current_message_id.is_some() {
                    messages_since_anchor = messages_since_anchor.saturating_add(1);
                    if messages_since_anchor >= anchor_refresh {
                        current_message_id = None;
                        messages_since_anchor = 0;
                    }
                } else {
                    // current_message_id is None — either we haven't posted a
                    // TUI frame yet or --clear just ran. Reset the counter
                    // so the *next* threshold cycle starts fresh.
                    messages_since_anchor = 0;
                }

                match outcome {
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
    name: std::sync::Arc<std::sync::Mutex<String>>,
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
                    ParsedInput::Command(SpecialCommand::Name(new_name)) => {
                        // Allow renaming while idle too — pre-`--new`, the
                        // user might want to label the upcoming session.
                        let trimmed = new_name.trim().to_string();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let display = {
                            let mut g = name.lock().unwrap();
                            *g = trimmed.clone();
                            trimmed
                        };
                        slack
                            .send_message(channel, &format!("🏷️ Session renamed to *{display}*."))
                            .await?;
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

#[allow(clippy::too_many_arguments)]
async fn handle_slack_message(
    msg: &IncomingMessage,
    input_tx: &mpsc::Sender<Vec<u8>>,
    pty: &mut PtyBackend,
    renderer: &mut TuiRenderer,
    slack: &SlackClient,
    channel: &str,
    current_message_id: &mut Option<String>,
    name: &std::sync::Arc<std::sync::Mutex<String>>,
    attach: Option<&AttachServer>,
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
            SpecialCommand::Name(new_name) => {
                let trimmed = new_name.trim().to_string();
                if trimmed.is_empty() {
                    return SlackOutcome::Continue;
                }
                let display = {
                    let mut g = name.lock().unwrap();
                    *g = trimmed.clone();
                    trimmed
                };
                // Update the attach window title in real time. Latches in
                // AttachServer so future-connecting clients also pick it up.
                if let Some(s) = attach {
                    s.set_title(build_title_frame(&display));
                }
                let _ = slack
                    .send_message(channel, &format!("🏷️ Session renamed to *{display}*."))
                    .await;
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
