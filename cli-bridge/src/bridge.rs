use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tracing::{error, info};

use bridge_auth::CredentialStore;
use bridge_core::commands::{
    ParsedInput, SpecialCommand, command_to_bytes, help_text, parse_input,
};
use bridge_core::dispatch::{self, DispatchHandle, LiveFrame};
use bridge_core::messaging::MessagingClient;
use bridge_core::terminal::TerminalBackend;
use bridge_core::types::{IncomingMessage, TerminalSize};
use bridge_pty::PtyBackend;
use bridge_slack::{SlackClient, SlackTranscriptFormat, TuiRenderer};

use crate::attach::{AttachEvent, AttachServer, open_attach_terminal, print_manual_attach_hint};
use crate::settings;

/// How often we drain the renderer and post to Slack. Slack's rate limit on
/// chat.postMessage is ~1 message/second per channel, so we tick a hair above
/// that to stay safely under it.
const RENDER_TICK: Duration = Duration::from_millis(1100);

/// How long to wait for the goodbye Slack post when the user hits Ctrl+C.
/// Best-effort: if the network is wedged we'd rather exit than hang.
const SHUTDOWN_POST_TIMEOUT: Duration = Duration::from_secs(2);

/// After the shell process exits, keep the session loop alive briefly to
/// drain any trailing PTY output (Windows ConPTY can keep emitting bytes
/// for a beat after the child is reaped). After this elapses, we break
/// out and tear down.
const SHELL_EXIT_GRACE: Duration = Duration::from_millis(500);

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
    pty_log_path: Option<String>,
    scroll_buffer_lines: usize,
    replace_block_chars: bool,
    show_cursor: bool,
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

    // Resolve channel: if `channel` looks like a Slack ID (C/G/D + ALL CAPS
    // alnum), use it verbatim. Otherwise treat it as a name and look it up
    // via the Slack API.
    let resolved_channel = if looks_like_channel_id(channel) {
        channel.to_string()
    } else {
        let name_clean = channel.trim_start_matches('#');
        info!("Resolving channel name '{name_clean}'…");
        slack
            .resolve_channel_name(name_clean)
            .await
            .with_context(|| format!("resolve channel name '{name_clean}'"))?
    };
    let channel: &str = &resolved_channel;

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

    // Done mutating the client. Wrap it in an Arc so each session's async
    // dispatch task can share it (the dispatcher posts to Slack off the hot
    // path; see bridge_core::dispatch).
    let slack = std::sync::Arc::new(slack);

    // If --pty-log was given, open it once for the whole bridge run. Each
    // session appends. Truncates on open so each cli-bridge invocation
    // starts fresh — easier to scope a reproduction.
    let pty_log_writer = match pty_log_path.as_deref() {
        Some(path) => {
            let f = std::fs::File::create(path)
                .with_context(|| format!("opening --pty-log file {path}"))?;
            info!("PTY output is being captured to {path}");
            Some(std::sync::Arc::new(std::sync::Mutex::new(f)))
        }
        None => None,
    };

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
            pty_log_writer.clone(),
            scroll_buffer_lines,
            replace_block_chars,
            show_cursor,
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
/// Heuristic: does the input look like a Slack channel/conversation ID?
/// Slack IDs are uppercase letter prefix (C public, G private/group, D DM)
/// followed by alphanumeric chars, typically 9–11 long. Channel *names*
/// are lowercase, may contain dashes/underscores, and start with a letter
/// or `#` — none of which match this pattern.
fn looks_like_channel_id(s: &str) -> bool {
    let s = s.trim();
    if s.len() < 5 {
        return false;
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap();
    if !matches!(first, 'C' | 'G' | 'D') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() && !c.is_ascii_lowercase())
}

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
    slack: &std::sync::Arc<SlackClient>,
    message_rx: &mut mpsc::Receiver<IncomingMessage>,
    channel: &str,
    shell: &str,
    size: TerminalSize,
    local: bool,
    anchor_refresh: u32,
    name: std::sync::Arc<std::sync::Mutex<String>>,
    pty_log: Option<std::sync::Arc<std::sync::Mutex<std::fs::File>>>,
    scroll_buffer_lines: usize,
    replace_block_chars: bool,
    show_cursor: bool,
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
    // The PTY exit signal. Wrapped in Option so we can `take()` it on first
    // fire — oneshot receivers panic if polled after they resolve, and we
    // want the surrounding select! to keep running for a short while after
    // exit to drain any trailing PTY output.
    let mut exit_rx = Some(handle.exit_rx);
    // Whether we've observed the shell exiting. Once true, we begin a short
    // grace period for trailing output and then break out of the session.
    let mut shell_exited_at: Option<tokio::time::Instant> = None;

    let mut renderer = TuiRenderer::with_scroll_buffer(size.cols, size.rows, scroll_buffer_lines);
    renderer.set_replace_block_chars(replace_block_chars);
    renderer.set_show_cursor(show_cursor);
    // Runtime-mutable settings, driven from Slack via `--config <key> <value>`.
    // The name field is the same Arc<Mutex<String>> the outer loop sees, so a
    // rename in the session is visible in the next between-session banner.
    let mut runtime_settings = settings::RuntimeSettings {
        anchor_refresh,
        name: name.clone(),
    };
    // Async Slack dispatch. All posting (live frame + the two-level scroll
    // buffer / history machinery) runs on a dedicated task fed over a bounded
    // channel, so the session loop never blocks on Slack's rate-limited
    // network calls — keeping PTY output and keystroke forwarding snappy. See
    // bridge_core::dispatch for the design.
    let dispatch = dispatch::spawn(
        channel.to_string(),
        slack.clone(),
        Box::new(SlackTranscriptFormat),
    );
    let mut tick = tokio::time::interval(RENDER_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Counter for the live-message re-anchor feature: every N inbound Slack
    // messages, anchor the transcript so the next TUI frame posts a NEW
    // message instead of editing the old one. This keeps the live frame
    // visible near the bottom of the channel as the user types. 0 disables.
    let mut messages_since_anchor: u32 = 0;
    // Whether we've posted any live frame yet this session. Tracks the
    // anchor-refresh "is_some()" gate the inline posting used to derive from
    // current_message_id (which now lives in the dispatcher task).
    let mut posted_live_frame = false;

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

        // Future that fires once when the child exits, then never again.
        // After firing we still want the select! to keep running long enough
        // to drain trailing output. Rather than re-take() the receiver each
        // tick, we just std::future::pending after the first fire, signalled
        // via shell_exited_at being Some.
        let exit_signal = async {
            if shell_exited_at.is_some() {
                std::future::pending::<()>().await;
            }
            match exit_rx.as_mut() {
                Some(rx) => {
                    let _ = rx.await;
                }
                None => std::future::pending::<()>().await,
            }
        };

        // Future that fires when the post-exit grace period elapses. Only
        // active once shell_exited_at is set.
        let exit_grace_done = async {
            match shell_exited_at {
                Some(t) => tokio::time::sleep_until(t + SHELL_EXIT_GRACE).await,
                None => std::future::pending::<()>().await,
            }
        };

        tokio::select! {
            // `biased`: shutdown branches (Ctrl+C, child exit, exit-grace
            // expiry) are checked first when multiple are ready. Without
            // this, tokio's random selection means a steady stream of PTY
            // output or attach traffic can starve the signal handler for
            // arbitrary long.
            biased;

            // Local Ctrl+C: end the whole bridge run. Highest priority so
            // the user can always interrupt. See teardown order below.
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl+C received, shutting down");
                // Drop both broadcast senders. Client writer tasks see
                // RecvError::Closed, send Goodbye, and exit; client
                // processes return and the spawned terminal windows close.
                let _ = attach.take();
                let _ = attach_output.take();
                // Kill the PTY child so the shell doesn't outlive us.
                let _ = pty.kill();
                break SessionEnd::Interrupted;
            }

            // Grace period elapsed after shell exit: tear down the session.
            _ = exit_grace_done, if shell_exited_at.is_some() => {
                info!("Shell exit grace elapsed, tearing down session");
                let _ = attach.take();
                let _ = attach_output.take();
                break SessionEnd::ShellExited;
            }

            // Child exited: mark the time so the grace timer starts. Don't
            // tear down yet — keep the loop alive for SHELL_EXIT_GRACE so
            // trailing PTY output (slow-to-flush on Windows ConPTY) makes it
            // into Slack and the attach window before we close everything.
            _ = exit_signal => {
                if shell_exited_at.is_none() {
                    info!("PTY child reaped");
                    exit_rx = None;
                    shell_exited_at = Some(tokio::time::Instant::now());
                }
            }

            // PTY output: forward to renderer + attach clients. None means
            // the shell process has exited — we drop the attach server (so
            // attached terminal windows close), flush trailing renderer
            // output to Slack, and end the session.
            data = output_rx.recv() => {
                match data {
                    Some(bytes) => {
                        // Capture raw PTY bytes if --pty-log is on. Best-
                        // effort: log a warning if the write fails, but
                        // never let it block real output processing.
                        if let Some(log) = pty_log.as_ref()
                            && let Ok(mut f) = log.lock()
                        {
                            use std::io::Write;
                            let _ = f.write_all(&bytes);
                        }
                        renderer.process(&bytes);
                        if let Some(tx) = attach_output.as_ref() {
                            let _ = tx.send(Arc::new(bytes));
                        }
                    }
                    None => {
                        info!("Shell output channel closed (shell exited)");
                        // Drop both broadcast senders so client tasks see
                        // RecvError::Closed and write Goodbye immediately.
                        let _ = attach.take();
                        let _ = attach_output.take();
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

            // Hand a fresh snapshot (scrolled-off rows + live frame) to the
            // dispatcher. Non-blocking: if the dispatcher is still busy with
            // the previous batch, we skip this tick and leave the rows in the
            // renderer's ring (bounded, oldest-evicting) for next time. This
            // is what keeps the loop responsive under a heavy output burst.
            _ = tick.tick() => {
                if dispatch.has_capacity() {
                    let rows = renderer.drain_scroll_buffer_rows();
                    let live = renderer.take_pending().map(|r| LiveFrame {
                        text: r.text,
                        is_edit: r.is_edit,
                    });
                    if live.as_ref().is_some_and(|l| l.is_edit) {
                        posted_live_frame = true;
                    }
                    dispatch.try_send_frame(rows, live);
                }
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
                    &dispatch,
                    &mut posted_live_frame,
                    &mut runtime_settings,
                    attach.as_ref(),
                ).await;

                // Re-anchor: every Nth inbound message, anchor the transcript
                // so the next TUI frame posts a NEW message instead of editing
                // the old one. This keeps the live frame near the bottom of the
                // channel so the user doesn't have to scroll back up to see it.
                // We only count once a live frame has actually been posted;
                // `--clear` (handled inside handle_slack_message) resets the
                // flag, so it implicitly resets this cycle too.
                if runtime_settings.anchor_refresh > 0 && posted_live_frame {
                    messages_since_anchor = messages_since_anchor.saturating_add(1);
                    if messages_since_anchor >= runtime_settings.anchor_refresh {
                        messages_since_anchor = 0;
                        posted_live_frame = false;
                        // Seal the active scroll buffer as history and force a
                        // fresh live message; clear the renderer's pending
                        // scroll buffer so we don't repeat already-posted
                        // history into the new message.
                        dispatch.anchor();
                        renderer.clear_scroll_buffer();
                    }
                } else {
                    // No live frame posted yet (or --clear just ran). Reset the
                    // counter so the *next* threshold cycle starts fresh.
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

    // Final flush: hand the dispatcher one last snapshot of anything still in
    // the renderer (trailing scroll-buffer rows + the final screen), then wait
    // for it to drain within the shutdown budget. Best-effort — if the network
    // is wedged we'd rather exit than hang.
    let rows = renderer.drain_scroll_buffer_rows();
    let live = renderer.take_pending().map(|r| LiveFrame {
        text: r.text,
        is_edit: r.is_edit,
    });
    dispatch.try_send_frame(rows, live);
    let _ = tokio::time::timeout(SHUTDOWN_POST_TIMEOUT, dispatch.shutdown()).await;

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
            biased;
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl+C while idle, exiting");
                return Ok(false);
            }
            msg = message_rx.recv() => {
                let Some(msg) = msg else {
                    return Ok(false);
                };
                match parse_input(&msg.text) {
                    // Between sessions, plain `--new` is fine — there's no
                    // shell to terminate. `--new force` works the same.
                    ParsedInput::Command(SpecialCommand::Restart { .. }) => return Ok(true),
                    ParsedInput::Command(SpecialCommand::Help { topic }) => {
                        let text = match topic.as_deref() {
                            Some("config") => settings::help_text(),
                            _ => help_text(),
                        };
                        slack.send_message(channel, &text).await?;
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
    dispatch: &DispatchHandle,
    posted_live_frame: &mut bool,
    runtime_settings: &mut settings::RuntimeSettings,
    attach: Option<&AttachServer>,
) -> SlackOutcome {
    let parsed = parse_input(&msg.text);
    match parsed {
        ParsedInput::Text(text) => {
            // Wrap pasted text in bracketed-paste markers, then send a
            // bare CR *outside* the brackets. Why:
            // - Modern TUI editors (Claude Code, helix, kitty's repl,
            //   etc.) enable bracketed paste mode (CSI ?2004h) and
            //   leave it on for the lifetime of the session. In that
            //   mode, terminals deliver pasted content surrounded by
            //   \x1b[200~ ... \x1b[201~ and the editor inserts that
            //   content into its buffer *without* interpreting any
            //   embedded \r as "submit".
            // - So if we just append \r to the text, the editor sees
            //   it as paste-content CR (visible as Ctrl-M in some
            //   editors) — Claude Code captures the line into its
            //   buffer but never submits it. Symptom: "I sent the
            //   message but Claude didn't respond until I hit Enter."
            // - Sending the CR *after* \x1b[201~ takes the editor out
            //   of paste mode first, then the CR is interpreted as
            //   Enter and the line submits.
            //
            // For shells that haven't enabled ?2004h (cmd, plain bash),
            // these markers are silently ignored — they show up as
            // unknown CSI escapes and get dropped. So this is safe to
            // do unconditionally.
            let mut bytes = Vec::with_capacity(text.len() + 13);
            bytes.extend_from_slice(b"\x1b[200~");
            bytes.extend_from_slice(text.as_bytes());
            bytes.extend_from_slice(b"\x1b[201~");
            bytes.push(b'\r');
            if input_tx.send(bytes).await.is_err() {
                error!("PTY input channel closed");
            }
            SlackOutcome::Continue
        }
        ParsedInput::Command(cmd) => match cmd {
            SpecialCommand::Kill => SlackOutcome::Kill,
            SpecialCommand::Restart { force } => {
                if force {
                    SlackOutcome::RestartOrNew
                } else {
                    // Shell is alive (we're in the session loop); require
                    // explicit `--new force` to terminate it. Stateless: no
                    // pending-confirmation flag to mismanage on retries.
                    dispatch.say(
                        "⚠️  A shell is already running. Reply `--new force` to terminate it and start a new one.",
                    );
                    SlackOutcome::Continue
                }
            }
            SpecialCommand::Resize(new_size) => {
                if let Err(e) = pty.resize(new_size) {
                    error!("Failed to resize PTY: {e}");
                }
                renderer.resize(new_size.cols, new_size.rows);
                dispatch.say(format!("📐 Resized to {}x{}", new_size.cols, new_size.rows));
                SlackOutcome::Continue
            }
            SpecialCommand::Clear => {
                // Anchor the transcript: seal the active scroll buffer as
                // history and start a fresh live message on next output. Clear
                // the renderer's pending scroll buffer so already-posted
                // history isn't repeated.
                dispatch.anchor();
                renderer.clear_scroll_buffer();
                *posted_live_frame = false;
                dispatch.say("📌 Anchored. Next output will start a fresh message.");
                SlackOutcome::Continue
            }
            SpecialCommand::Help { topic } => {
                let text = match topic.as_deref() {
                    Some("config") => settings::help_text(),
                    _ => help_text(),
                };
                dispatch.say(text);
                SlackOutcome::Continue
            }
            SpecialCommand::Name(new_name) => {
                // Pass through the settings registry so the value lives
                // in exactly one place; --config name <x> and --name <x>
                // are now strict synonyms.
                match settings::apply_setting("name", &new_name, renderer, runtime_settings) {
                    Ok(_) => {
                        let display = settings::read_setting("name", renderer, runtime_settings)
                            .unwrap_or_default();
                        if let Some(s) = attach {
                            s.set_title(build_title_frame(&display));
                        }
                        dispatch.say(format!("🏷️ Session renamed to *{display}*."));
                    }
                    Err(e) => {
                        dispatch.say(format!("⚠️ {e}"));
                    }
                }
                SlackOutcome::Continue
            }
            SpecialCommand::Config { key, value } => {
                let reply = match (key, value) {
                    (None, _) => settings::list_settings(renderer, runtime_settings),
                    (Some(k), None) => match settings::read_setting(&k, renderer, runtime_settings)
                    {
                        Some(v) => match settings::lookup(&k) {
                            Some(meta) => format!(
                                "`{}` = `{v}` _({})_\n_{}_",
                                meta.name, meta.kind, meta.description
                            ),
                            None => format!("`{k}` = `{v}`"),
                        },
                        None => {
                            format!("⚠️ unknown setting `{k}`. Try `--help config` for the list.")
                        }
                    },
                    (Some(k), Some(v)) => {
                        match settings::apply_setting(&k, &v, renderer, runtime_settings) {
                            Ok(msg) => {
                                // Special-case name: keep the attach title in sync.
                                if k == "name"
                                    && let Some(s) = attach
                                {
                                    let display =
                                        settings::read_setting("name", renderer, runtime_settings)
                                            .unwrap_or_default();
                                    s.set_title(build_title_frame(&display));
                                }
                                format!("✅ {msg}")
                            }
                            Err(e) => format!("⚠️ {e}"),
                        }
                    }
                };
                dispatch.say(reply);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_id_recognized() {
        assert!(looks_like_channel_id("C0123456789"));
        assert!(looks_like_channel_id("G017KTQLT5M"));
        assert!(looks_like_channel_id("D01ABCDEF12"));
    }

    #[test]
    fn channel_name_not_id() {
        // Names start lowercase or with `#`; never a bare uppercase ID prefix.
        assert!(!looks_like_channel_id("general"));
        assert!(!looks_like_channel_id("#general"));
        assert!(!looks_like_channel_id("dev-team"));
        // Lowercase letters anywhere disqualify (Slack IDs are uppercase).
        assert!(!looks_like_channel_id("CabC123"));
        // Wrong leading letter.
        assert!(!looks_like_channel_id("XABCDEFGH"));
        // Too short.
        assert!(!looks_like_channel_id("CXY"));
    }
}
