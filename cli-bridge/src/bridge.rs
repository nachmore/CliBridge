use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tracing::{debug, error, info};

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

/// Per-message size budget. Slack has *two* relevant ceilings:
///
/// 1. **API hard cap (`chat.update` `msg_too_long`):** ~4,000 UTF-16
///    code units. Above this Slack rejects the call.
/// 2. **Client rendering cap (~3,000 chars):** Slack's desktop/web
///    client renders the tail of any `text` field above ~3,000 chars
///    as a collapsed "Show more" attachment, which displays as a
///    *separate* stacked bubble. The API succeeds, but the user sees
///    one logical message rendered as two — breaking our fenced-code
///    formatting because the closing ``` lands in the second bubble.
///
/// We target a ceiling well under (2) so each bridge message is
/// guaranteed to render as a single bubble. Headroom for labels/
/// headers (📜 *Scroll buffer* etc.) and a margin against UTF-16 width
/// surprises.
///
/// Always measure with [`slack_text_size`] when comparing against this
/// constant — emoji like 📜 / 🟢 / 🌉 are each one Rust `char` but
/// two UTF-16 units, and an earlier `chars().count()` check let
/// emoji-heavy bodies sneak past, after which the edit silently got
/// truncated mid-content with no closing fence.
const SLACK_MESSAGE_CHAR_LIMIT: usize = 2_800;

/// Count UTF-16 code units in a string — the unit Slack actually uses
/// for its `msg_too_long` check. For ASCII this is identical to byte
/// count and to `chars().count()`; for emoji on the supplementary plane
/// (📜, 🟢, 🌉, …) each `char` becomes two units.
fn slack_text_size(s: &str) -> usize {
    s.encode_utf16().count()
}

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
    slack: &SlackClient,
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
    let mut current_message_id: Option<String> = None;
    // The last body we posted/edited as the live message. Used to skip
    // wasteful identical edits — the renderer now always produces a
    // frame each tick (so we don't miss frames where the live screen
    // changed while no `process` call ran), but we only actually call
    // chat.update when the rendered text differs from this.
    let mut last_live_body: Option<String> = None;
    // Two-level scroll buffer: an "active" 📜 Scroll buffer message we extend
    // each tick by editing in place, plus an implicit collection of
    // already-locked 📚 History messages above it (those we never touch
    // again). When the active fills up we relabel it as History and start
    // a fresh active. See `extend_or_lock_active` for the policy.
    let mut active_scroll_buffer: Option<ActiveScrollBuffer> = None;
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
                // Best-effort post any pending output (scroll buffer + live
                // frame) to Slack within the shutdown budget.
                let _ = tokio::time::timeout(
                    SHUTDOWN_POST_TIMEOUT,
                    drain_and_post(slack, channel, &mut renderer, &mut current_message_id, &mut active_scroll_buffer, &mut last_live_body),
                )
                .await;
                break SessionEnd::Interrupted;
            }

            // Grace period elapsed after shell exit: tear down the session.
            _ = exit_grace_done, if shell_exited_at.is_some() => {
                info!("Shell exit grace elapsed, tearing down session");
                let _ = attach.take();
                let _ = attach_output.take();
                drain_and_post(slack, channel, &mut renderer, &mut current_message_id, &mut active_scroll_buffer, &mut last_live_body).await;
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
                        // RecvError::Closed and write Goodbye immediately —
                        // before we block on the trailing Slack post.
                        let _ = attach.take();
                        let _ = attach_output.take();

                        drain_and_post(slack, channel, &mut renderer, &mut current_message_id, &mut active_scroll_buffer, &mut last_live_body).await;
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

            // Drain pending scroll buffer + live frame and post.
            _ = tick.tick() => {
                drain_and_post(slack, channel, &mut renderer, &mut current_message_id, &mut active_scroll_buffer, &mut last_live_body).await;
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
                    &mut active_scroll_buffer,
                    &mut runtime_settings,
                    attach.as_ref(),
                ).await;

                // Re-anchor: every Nth inbound message, force the next TUI
                // frame to post a NEW message instead of editing the old one.
                // This keeps the live frame near the bottom of the channel
                // so the user doesn't have to scroll back up to see it.
                // The `--clear` command (handled inside handle_slack_message)
                // already nulls current_message_id, so we treat that path as
                // an implicit anchor reset by checking is_some() first.
                if runtime_settings.anchor_refresh > 0 && current_message_id.is_some() {
                    messages_since_anchor = messages_since_anchor.saturating_add(1);
                    if messages_since_anchor >= runtime_settings.anchor_refresh {
                        // last_live_body invalidates automatically next
                        // tick (drain_and_post clears when current_message_id is None).
                        current_message_id = None;
                        messages_since_anchor = 0;
                        // The new message starts with a clean scroll buffer so
                        // we don't repeat history that was already in the
                        // previous (now-frozen) message. Also lock the
                        // active scroll buffer — it'll sit above the new live
                        // message and must not be edited further.
                        renderer.clear_scroll_buffer();
                        if let Some(active) = active_scroll_buffer.take() {
                            lock_active_scroll_buffer(slack, channel, active).await;
                        }
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
    slack: &SlackClient,
    channel: &str,
    current_message_id: &mut Option<String>,
    active_scroll_buffer: &mut Option<ActiveScrollBuffer>,
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
                    let _ = slack
                        .send_message(
                            channel,
                            "⚠️  A shell is already running. Reply `--new force` to terminate it and start a new one.",
                        )
                        .await;
                    SlackOutcome::Continue
                }
            }
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
                renderer.clear_scroll_buffer();
                if let Some(active) = active_scroll_buffer.take() {
                    lock_active_scroll_buffer(slack, channel, active).await;
                }
                let _ = slack
                    .send_message(
                        channel,
                        "📌 Anchored. Next output will start a fresh message.",
                    )
                    .await;
                SlackOutcome::Continue
            }
            SpecialCommand::Help { topic } => {
                let text = match topic.as_deref() {
                    Some("config") => settings::help_text(),
                    _ => help_text(),
                };
                let _ = slack.send_message(channel, &text).await;
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
                        let _ = slack
                            .send_message(channel, &format!("🏷️ Session renamed to *{display}*."))
                            .await;
                    }
                    Err(e) => {
                        let _ = slack.send_message(channel, &format!("⚠️ {e}")).await;
                    }
                }
                SlackOutcome::Continue
            }
            SpecialCommand::Config { key, value } => {
                let reply = match (key, value) {
                    (None, _) => settings::list_settings(renderer, runtime_settings),
                    (Some(k), None) => match settings::read_setting(&k, renderer, runtime_settings) {
                        Some(v) => match settings::lookup(&k) {
                            Some(meta) => format!(
                                "`{}` = `{v}` _({})_\n_{}_",
                                meta.name, meta.kind, meta.description
                            ),
                            None => format!("`{k}` = `{v}`"),
                        },
                        None => format!(
                            "⚠️ unknown setting `{k}`. Try `--help config` for the list."
                        ),
                    },
                    (Some(k), Some(v)) => match settings::apply_setting(
                        &k,
                        &v,
                        renderer,
                        runtime_settings,
                    ) {
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
                    },
                };
                let _ = slack.send_message(channel, &reply).await;
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

/// One open scroll buffer message that the bridge keeps extending each tick
/// by editing in place. When the body grows past
/// `SLACK_MESSAGE_CHAR_LIMIT`, the bridge edits it once more to relabel
/// it as 📚 *History* — sealing it — and lets `active_scroll_buffer` drop
/// to `None` so the next tick opens a fresh active.
///
/// `body` is the *full* current text of the message (including header
/// and trailing fence) so we can do an unambiguous size check before
/// committing to extend vs. lock.
struct ActiveScrollBuffer {
    message_id: String,
    body: String,
}

/// Render a 📜 *Scroll buffer* message body containing the given rows.
fn render_active_body(rows_text: &str) -> String {
    format!("📜 *Scroll buffer*\n```\n{rows_text}```")
}

/// Demote `active` to a sealed 📚 *History* message by editing it once
/// more with the History label and the same content body. Best-effort:
/// on edit failure we log and move on — the user still sees the prior
/// 📜 *Scroll buffer* label, which is wrong but harmless.
async fn lock_active_scroll_buffer(slack: &SlackClient, channel: &str, active: ActiveScrollBuffer) {
    // The body still has the 📜 *Scroll buffer* prefix; swap it for the
    // 📚 *History* label. We keep the inner rows verbatim — same content,
    // new label.
    let locked_body = active
        .body
        .replacen("📜 *Scroll buffer*", "📚 *History*", 1);
    debug!("Locking scroll buffer ts={} as History", active.message_id);
    if let Err(e) = slack
        .edit_message(channel, &active.message_id, &locked_body)
        .await
    {
        error!("Failed to lock scroll buffer message as History: {e}");
    }
}

/// Drain the renderer into Slack using the **two-level log-segment** model:
///
/// 1. If any rows have scrolled off the virtual terminal since the last
///    tick, append them to the active 📜 *Scroll buffer* message:
///    - If there's no active yet but there *is* a prior live message,
///      reuse that live message: edit it in place with just the
///      scrolled-off rows under a 📜 *Scroll buffer* header. (This kills
///      the stale 🟢 *Live* label and avoids posting another message.)
///    - If there's no active and no live, post a fresh active.
///    - If there's an active, edit it to append the new rows.
///    - If extending would push it past the size budget, lock the
///      current active as 📚 *History* and start a fresh active.
/// 2. Post or edit the new live message with the current screen.
///
/// Each row therefore lives in exactly one Slack message — scrolled-off
/// content in either an active 📜 Scroll buffer or a locked 📚 History
/// message, on-screen content in the live message — with the duplication
/// problem ruled out structurally and tiny "3-line scroll buffer" messages
/// avoided by extending the active rather than posting fresh each tick.
async fn drain_and_post(
    slack: &SlackClient,
    channel: &str,
    renderer: &mut TuiRenderer,
    current_message_id: &mut Option<String>,
    active_scroll_buffer: &mut Option<ActiveScrollBuffer>,
    last_live_body: &mut Option<String>,
) {
    // If we have no current live message (start of session, --clear,
    // anchor-refresh, or a recent demotion), the dedupe state for the
    // *previous* live message is meaningless. Drop it so the next post
    // happens fresh.
    if current_message_id.is_none() {
        *last_live_body = None;
    }

    let pending = renderer.scroll_buffer_pending();
    if pending > 0 {
        // Each pass either:
        //  - extends the active with as many lines as fit, OR
        //  - rolls over to a fresh active (locking the current one)
        //    when not even one line fits in the active's remaining
        //    budget, OR
        //  - posts a fresh active when there is no current active.
        //
        // We do at most ONE Slack write per tick to keep per-tick work
        // bounded under the rate-limiter's ~1.1s/call budget; whatever
        // doesn't fit stays in the renderer's ring for the next tick.
        // **No row is ever dropped** — earlier versions did, when the
        // active was near-full and the next line couldn't fit; that
        // path now triggers a rollover instead.
        flush_one_scroll_buffer_batch(slack, channel, renderer, active_scroll_buffer, current_message_id).await;
        // If the flush demoted our live message into the active
        // scroll buffer, the body we tracked as "last live" no longer
        // exists as a live message. Drop it so the next render posts
        // fresh instead of being deduped against a stale memory.
        if current_message_id.is_none() {
            *last_live_body = None;
        }

        if renderer.scroll_buffer_pending() > 0 {
            debug!(
                "Scroll buffer backlog: {} rows still pending after this tick",
                renderer.scroll_buffer_pending()
            );
        }
    }

    let Some(rendered) = renderer.take_pending() else {
        return;
    };
    // Dedupe identical live frames: the renderer always emits the
    // current frame each tick (so we catch screen changes that
    // happened with no new `process` call between ticks — e.g. the
    // tail end of a long burst sitting on screen after the agent has
    // gone quiet), but most ticks produce the same body as last time
    // and don't need a Slack edit. Skip if unchanged.
    if rendered.is_edit && last_live_body.as_deref() == Some(rendered.text.as_str()) {
        return;
    }
    post_or_edit(slack, channel, &rendered, current_message_id).await;
    if rendered.is_edit {
        *last_live_body = Some(rendered.text);
    }
}

/// Do one unit of scroll buffer work: either extend the active with as
/// many lines as fit, or roll over to a fresh active. Bounds per-tick
/// Slack work to a single edit (or a single post + a lock-edit, when
/// rolling over). Returns when there's nothing pending or one Slack
/// write has been issued.
async fn flush_one_scroll_buffer_batch(
    slack: &SlackClient,
    channel: &str,
    renderer: &mut TuiRenderer,
    active_scroll_buffer: &mut Option<ActiveScrollBuffer>,
    current_message_id: &mut Option<String>,
) {
    if renderer.scroll_buffer_pending() == 0 {
        return;
    }

    // A fresh active body starts with `📜 *Scroll buffer*\n```\n` plus a
    // closing "```" — call that the framing overhead. Empirically ~24
    // UTF-16 units; recompute exactly so a future header tweak can't
    // silently desync.
    let fresh_overhead = slack_text_size("📜 *Scroll buffer*\n```\n```");

    // Available budget inside the active (or in a fresh active if
    // there isn't one yet).
    let available = match active_scroll_buffer {
        Some(a) => SLACK_MESSAGE_CHAR_LIMIT.saturating_sub(slack_text_size(&a.body)),
        None => SLACK_MESSAGE_CHAR_LIMIT.saturating_sub(fresh_overhead),
    };

    // If the active is too full to fit even the next line, lock it
    // first. The next call (this tick or next) will then post a fresh
    // active for the pending rows.
    let next_line_size = renderer
        .peek_first_scroll_buffer_row()
        .map(slack_text_size_of_row)
        .unwrap_or(0);
    if next_line_size > available
        && let Some(active) = active_scroll_buffer.take()
    {
        debug!(
            "Active scroll buffer ts={} can't fit next line ({} > {}); locking as 📚 History",
            active.message_id, next_line_size, available
        );
        lock_active_scroll_buffer(slack, channel, active).await;
        return;
    }

    // Drain as many lines as fit into the available budget.
    let budget = if active_scroll_buffer.is_some() {
        available
    } else {
        SLACK_MESSAGE_CHAR_LIMIT.saturating_sub(fresh_overhead)
    };
    let batch = drain_lines_into_budget(renderer, budget);
    if batch.is_empty() {
        // The next pending row is bigger than even a fresh message can
        // hold (a single line > ~2,776 units, which is wider than any
        // realistic terminal). Hard-split at a char boundary so we make
        // forward progress; a degenerately-wide line is content too.
        if let Some(row) = renderer.pop_first_scroll_buffer_row() {
            let mut line: String = row.into_iter().collect();
            line.push('\n');
            let split = char_boundary_at_utf16(&line, budget);
            let head = line[..split].to_string();
            let tail: String = line[split..].to_string();
            // Push the tail back at the front of the ring as a single
            // row (without its trailing newline) so we resume next tick.
            let tail_chars: Vec<char> = tail.trim_end_matches('\n').chars().collect();
            if !tail_chars.is_empty() {
                renderer.push_front_scroll_buffer_row(tail_chars);
            }
            error!(
                "Scroll buffer row wider than per-message budget; hard-split at {} units",
                slack_text_size(&head)
            );
            ingest_scroll_buffer_batch(
                slack,
                channel,
                current_message_id,
                active_scroll_buffer,
                &head,
            )
            .await;
        }
        return;
    }
    ingest_scroll_buffer_batch(
        slack,
        channel,
        current_message_id,
        active_scroll_buffer,
        &batch,
    )
    .await;
}

/// UTF-16 size of a row including its trailing newline. Convenience —
/// keeps the size calc symmetric with how `drain_lines_into_budget`
/// constructs lines for inclusion.
fn slack_text_size_of_row(row: &[char]) -> usize {
    let mut size = 0;
    for &c in row {
        size += c.len_utf16();
    }
    size + 1 // for '\n'
}

/// Drain rows from the renderer (oldest first) and accumulate their
/// rendered lines until the next line wouldn't fit in `budget` UTF-16
/// units. Lines that don't fit stay in the renderer for next tick.
/// Never drops rows.
fn drain_lines_into_budget(renderer: &mut TuiRenderer, budget: usize) -> String {
    let mut accum = String::new();
    let mut accum_size = 0usize;
    while let Some(peek) = renderer.peek_first_scroll_buffer_row() {
        let line_size = slack_text_size_of_row(peek);
        if accum_size + line_size > budget {
            break;
        }
        // Commit.
        let row = renderer.pop_first_scroll_buffer_row().unwrap();
        for c in row {
            accum.push(c);
        }
        accum.push('\n');
        accum_size += line_size;
    }
    accum
}

/// Find the largest byte index `<= s.len()` whose prefix `s[..idx]`
/// has exactly `units` or fewer UTF-16 code units AND is a valid char
/// boundary. Used for hard-splitting an over-wide line so we never
/// produce invalid UTF-8.
fn char_boundary_at_utf16(s: &str, units: usize) -> usize {
    let mut consumed = 0usize;
    for (i, c) in s.char_indices() {
        let next = consumed + c.len_utf16();
        if next > units {
            return i;
        }
        consumed = next;
    }
    s.len()
}

/// Apply a batch of scrolled-off rows (already concatenated as one
/// text blob with trailing '\n's per row) to the active scroll-buffer
/// message, extending it via edit when it fits and locking + rolling
/// over when it doesn't. The caller is responsible for ensuring the
/// batch fits in a single Slack edit — see [`flush_one_scroll_buffer_batch`].
///
/// Also handles the "no active, but a prior live anchor exists" case
/// by demoting the live message to active scroll buffer.
async fn ingest_scroll_buffer_batch(
    slack: &SlackClient,
    channel: &str,
    current_message_id: &mut Option<String>,
    active_scroll_buffer: &mut Option<ActiveScrollBuffer>,
    batch: &str,
) {
    // Case A: no active scroll buffer yet. Either reuse the live message
    // (demote it to scroll buffer) or post a fresh active.
    if active_scroll_buffer.is_none() {
        let body = render_active_body(batch);
        if let Some(msg_id) = current_message_id.take() {
            match slack.edit_message(channel, &msg_id, &body).await {
                Ok(()) => {
                    debug!("Demoted live message ts={msg_id} into active 📜 Scroll buffer");
                    *active_scroll_buffer = Some(ActiveScrollBuffer {
                        message_id: msg_id,
                        body,
                    });
                    return;
                }
                Err(e) => {
                    error!(
                        "Failed to demote live message to scroll buffer, posting fresh: {e}"
                    );
                    // Fall through.
                }
            }
        }
        match slack.send_message(channel, &body).await {
            Ok(ts) => {
                debug!("Opened new active 📜 Scroll buffer ts={ts}");
                *active_scroll_buffer = Some(ActiveScrollBuffer {
                    message_id: ts,
                    body,
                });
            }
            Err(e) => error!("Failed to post new active scroll buffer: {e}"),
        }
        return;
    }

    // Case B: extend the existing active. If it would overflow Slack's
    // edit cap, lock the current one and roll into a fresh active for
    // the new rows. We measure with slack_text_size (UTF-16 units) — the
    // unit Slack's msg_too_long check actually uses; an earlier version
    // counted Rust `chars` and let emoji-heavy bodies sneak past, after
    // which Slack truncated the edit mid-content with no closing fence.
    let active = active_scroll_buffer.as_mut().unwrap();
    let extended = build_extended_active_body(&active.body, batch);
    let extended_size = slack_text_size(&extended);
    if extended_size <= SLACK_MESSAGE_CHAR_LIMIT {
        match slack
            .edit_message(channel, &active.message_id, &extended)
            .await
        {
            Ok(()) => {
                active.body = extended;
                return;
            }
            Err(e) => {
                // The edit failed — most plausibly msg_too_long under
                // a model mismatch we haven't accounted for. Don't keep
                // a half-broken active around: lock what we have and
                // start fresh with the new rows below. This is the
                // safety-net path for "extension overflow we didn't
                // predict"; the predicted path is the else branch.
                error!(
                    "Failed to extend active scroll buffer ts={} (extended size {}, budget {}): {e}; rolling over",
                    active.message_id, extended_size, SLACK_MESSAGE_CHAR_LIMIT
                );
                // Fall through to rollover.
            }
        }
    } else {
        debug!(
            "Active scroll buffer ts={} would exceed {} units (extended size {}); locking as 📚 History and rolling over",
            active.message_id, SLACK_MESSAGE_CHAR_LIMIT, extended_size
        );
    }

    // Rollover path: lock the current active, then open a fresh one
    // containing just the new rows. (Don't try to demote anything here
    // — the live message, if any, stays live; we just post a new active
    // scroll buffer below it.)
    let locked = active_scroll_buffer.take().unwrap();
    lock_active_scroll_buffer(slack, channel, locked).await;

    let body = render_active_body(batch);
    match slack.send_message(channel, &body).await {
        Ok(ts) => {
            debug!("Opened new active 📜 Scroll buffer ts={ts} after rollover");
            *active_scroll_buffer = Some(ActiveScrollBuffer {
                message_id: ts,
                body,
            });
        }
        Err(e) => error!("Failed to post fresh active scroll buffer after rollover: {e}"),
    }
}

/// Build the new full body of an active 📜 Scroll buffer after appending
/// `new_rows_text`. The existing body has the form:
///   "📜 *Scroll buffer*\n```\n<rows>```"
/// We splice the new rows immediately before the trailing fence.
fn build_extended_active_body(existing: &str, new_rows_text: &str) -> String {
    // Strip the trailing "```" so we can append more rows then re-add it.
    let trimmed = existing.strip_suffix("```").unwrap_or(existing);
    format!("{trimmed}{new_rows_text}```")
}

/// Post a fresh message, or edit `current_message_id` for TUI frames.
/// Falls back to a fresh post if editing fails for any reason — including
/// `msg_too_long`, which shouldn't happen with the log-segment model
/// (live frame is bounded by screen size) but is logged as a heads-up
/// rather than swallowed silently.
async fn post_or_edit(
    slack: &SlackClient,
    channel: &str,
    rendered: &RenderedOutput,
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
                error!("Failed to edit live message, posting fresh: {e}");
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

    #[test]
    fn extended_active_body_appends_before_fence() {
        let original = render_active_body("first\nsecond\n");
        let extended = build_extended_active_body(&original, "third\n");
        assert_eq!(
            extended,
            "📜 *Scroll buffer*\n```\nfirst\nsecond\nthird\n```"
        );
    }

    #[test]
    fn extended_active_body_round_trips() {
        // Successive extensions produce a single coherent message body.
        let mut body = render_active_body("a\n");
        body = build_extended_active_body(&body, "b\n");
        body = build_extended_active_body(&body, "c\n");
        assert_eq!(body, "📜 *Scroll buffer*\n```\na\nb\nc\n```");
    }

    #[test]
    fn drain_lines_into_budget_never_drops() {
        // Regression: an earlier version dropped rows when the next
        // line couldn't fit even in a fresh budget — happened when the
        // active scroll buffer was near-full and `available` was tiny.
        // Correct behavior: leave the row in the renderer, return what
        // we have. The caller's rollover path picks it up next pass.
        let mut renderer = TuiRenderer::with_scroll_buffer(20, 2, 50);
        // Trigger TUI mode so the renderer captures scrolled-off rows.
        renderer.process(b"\x1b[2J");
        for i in 0..5 {
            renderer.process(format!("line{i}\r\n").as_bytes());
        }
        let pending_before = renderer.scroll_buffer_pending();
        // Budget too small for even one line — drain returns "" and
        // leaves the row in place.
        let out = drain_lines_into_budget(&mut renderer, 1);
        assert_eq!(out, "");
        assert_eq!(
            renderer.scroll_buffer_pending(),
            pending_before,
            "rows must not be dropped on tight budget"
        );
    }

    #[test]
    fn drain_lines_into_budget_takes_what_fits() {
        let mut renderer = TuiRenderer::with_scroll_buffer(20, 2, 50);
        // Trigger TUI mode so the renderer captures scrolled-off rows.
        renderer.process(b"\x1b[2J");
        for i in 0..5 {
            renderer.process(format!("line{i}\r\n").as_bytes());
        }
        // "line0\n" is 6 chars; budget 20 should fit ~3 lines.
        let out = drain_lines_into_budget(&mut renderer, 20);
        assert!(out.contains("line0"));
        assert!(out.contains("line1"));
        assert!(out.contains("line2"));
        // Remaining lines stay in the ring.
        assert!(renderer.scroll_buffer_pending() > 0);
    }

    #[test]
    fn char_boundary_at_utf16_handles_emoji() {
        // 📜 is 1 char / 2 UTF-16 units. With units=1 we can't fit it
        // — boundary lands at byte 0.
        assert_eq!(char_boundary_at_utf16("📜x", 1), 0);
        // With units=2 we fit the emoji exactly.
        assert_eq!(char_boundary_at_utf16("📜x", 2), "📜".len());
        // ASCII string.
        assert_eq!(char_boundary_at_utf16("abcdef", 3), 3);
    }

    #[test]
    fn slack_text_size_counts_utf16_units() {
        // ASCII: identical to byte/char count.
        assert_eq!(slack_text_size("hello"), 5);
        // Emoji on the supplementary plane: 1 Rust char = 2 UTF-16 units.
        // 📜 alone:
        assert_eq!("📜".chars().count(), 1);
        assert_eq!(slack_text_size("📜"), 2);
        // The render_active_body header has 📜 (2 units) plus 17 ASCII
        // units = 19. (" *Scroll buffer*\n" plus the leading space).
        assert_eq!(slack_text_size("📜 *Scroll buffer*\n"), 19);
    }
}
