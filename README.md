# CliBridge

Bridge your local CLI to Slack. Drive a real shell from a Slack channel — type
commands in Slack, watch output stream back, and run TUI apps (vim, htop,
Claude Code) inside an emulated screen that renders live in a code block.
Optionally also see the same shell in a fresh local terminal window that
mirrors everything bidirectionally.

## Features

- **Bidirectional shell over Slack** — type commands in a channel, see output
  streamed and re-rendered as it changes.
- **TUI emulation** — full virtual screen with ANSI escape support
  (CSI movement, ECH/DCH/ICH/IL/DL, scroll regions, save/restore cursor,
  pending wrap, OSC, DCS); the live frame edits a single Slack message,
  rate-limited to 1 update/sec.
- **Local terminal mirror** — auto-spawns a real OS terminal that mirrors the
  bridge. Type locally too; both Slack and the local window show the same
  shell.
- **Scroll buffer** — content that scrolls off the live frame is posted into
  the Slack channel as a 📜 *Scroll buffer* message that fills as new rows
  arrive; once full it locks as 📚 *History* and a fresh scroll buffer
  opens. The live message never grows past Slack's chat.update size limit.
- **Browser-based login** — embedded WebView captures your `xoxc-` token + `d`
  cookie. No bot setup needed.
- **Channel by name OR ID** — pass `--channel general` and we look it up,
  or `--channel-id C0123456789` to skip the API roundtrip.
- **Cross-platform** — Windows (ConPTY), macOS (Unix PTY + Terminal.app
  launcher), Linux (Unix PTY + GNOME Terminal/konsole/alacritty/xterm).

## Quick Start

### 1. Sign in to Slack

```
cargo run -- --login
```

A WebView2 window opens at `slack.com/signin`. Sign in normally; the window
captures your token + cookies on the first authenticated API call and saves
them to `~/.config/cli-bridge/credentials.json` (or the platform equivalent).

### 2. Run the bridge

```
cargo run -- --workspace acme --channel general
```

(Substitute your workspace name as saved by `--login` and the channel name
or ID you want to bridge to.) A new local terminal window opens running your
shell; everything you type there or in Slack ends up in the same shell.

For a release build:

```
cargo build --release
./target/release/cli-bridge --workspace acme --channel general
```

## CLI Usage

```
cli-bridge [OPTIONS]

  -c, --channel <CHANNEL>      Slack channel — accepts either an ID
                               (e.g. C0123456789, G…, D…) or a name
                               (e.g. general or #general). IDs are used
                               verbatim; names are resolved via the Slack
                               API at startup.

Session:
  -w, --workspace <WORKSPACE>  Workspace name (saved by --login). If only one
                               workspace is saved, this is optional.
  -s, --shell <SHELL>          Shell to spawn (default: cmd.exe on Windows,
                               $SHELL on macOS/Linux).
      --name <NAME>            Display name for this session. Shows up in
                               banners and the local terminal title.
                               Default: "CliBridge".

Display:
      --cols <COLS>            Terminal width in columns. Default: 120.
      --rows <ROWS>            Terminal height in rows. Default: 24.
      --scroll-buffer <N>      Scroll buffer lines retained. Default: 10000.
                               0 disables. (alias: --scrollback)
      --anchor-refresh <N>     Re-anchor the live message every N inbound
                               Slack messages (default: 10, 0 to disable).
      --no-local               Skip auto-opening a local terminal mirror.
      --replace-block-chars    Replace U+2580–U+259F (█ ▌ ▐ ▛ etc.) with
                               spaces in Slack output. Slack's font
                               fallback renders these wider than one cell
                               and pushes box-drawing layouts (e.g. the
                               Claude Code banner) out of column. The
                               local attach window is unaffected.

Commands:
      --login                  Open a browser to sign in to Slack and save
                               credentials.
      --list-workspaces        List saved workspaces.

Slack:
      --url <URL>              Slack API base URL (for enterprise grids
                               that don't auto-derive from the workspace
                               URL — usually unneeded).

Debug:
      --pty-log <PATH>         Capture every byte of PTY output to file for
                               offline replay via the pty_replay example.
      --config <PATH>          Path to a TOML config file.
  -h, --help                   Print help.
```

## Special Commands

Type these in the Slack channel. Prefix is `--` (not `/`, because Slack
intercepts slash commands client-side).

| Command | Action |
|---------|--------|
| `--ctrl+c` | Send interrupt (SIGINT) |
| `--ctrl+d` | Send EOF |
| `--ctrl+z` | Suspend (SIGTSTP) |
| `--ctrl+l` | Clear screen |
| `--ctrl+\` | Send SIGQUIT |
| `--kill` | Kill the shell process |
| `--restart` / `--new` | (Re)spawn the shell. While alive, asks for confirmation; reply `--new force` to proceed. After exit, plain `--new` works. |
| `--resize 120x40` | Resize terminal (cols x rows) |
| `--clear` | End the current live message and start a new one on next output |
| `--tab` | Send Tab |
| `--esc` | Send Escape |
| `--up` `--down` `--left` `--right` | Arrow keys |
| `--tmux <key>` | Send tmux prefix (Ctrl+B) + key |
| `--raw <hex>` | Send raw bytes (hex-encoded) |
| `--slash <name>` | Send a literal `/name` to the shell (e.g. `--slash init` for Claude Code) |
| `--name <text>` | Rename the session (updates banners + local terminal title) |
| `--help` | Show this help |

Any other text is sent verbatim to the shell, with a CR appended so the line
is submitted.

## Local terminal mirror

Default-on. The bridge opens a fresh terminal window — Windows Terminal if
installed, falling back to `cmd /c start`; Terminal.app on macOS via
`osascript`; gnome-terminal/konsole/alacritty/xterm on Linux — running
`cli-bridge --attach 127.0.0.1:<port> --attach-token <secret>`. That client
process puts the local terminal into raw mode and forwards bytes both ways.

- Ctrl+C in the **mirror** window → kills the foreground shell job.
- Ctrl+C in the **bridge** window → quits the bridge entirely.
- When the shell exits (you type `exit`, the process dies), the mirror
  window closes and the bridge waits for `--new` from Slack.
- Pass `--no-local` to skip the mirror (Slack-only).

## Configuration

CliBridge looks for config in this order:
1. `--config <path>` CLI argument
2. `./cli-bridge.toml` (current directory)
3. `~/.config/cli-bridge/config.toml` (or platform equivalent)

See [`cli-bridge.example.toml`](cli-bridge.example.toml) for all options.

## Logging

`RUST_LOG` controls verbosity. `info` is the default. Useful filters:

- `RUST_LOG=cli_bridge=debug,bridge_slack=debug` — bridge + renderer detail,
  no noise from `hyper`/`reqwest`.
- `RUST_LOG=bridge_slack=trace` — every CSI dispatch with cursor before/after.
  Pair with `--pty-log pty.bin` and the `pty_replay` example to debug
  rendering issues offline.

## Architecture

```
┌────────────────────────────────────────────────────────────┐
│                       cli-bridge                            │
│                     (binary crate)                          │
├────────────────────────────────────────────────────────────┤
│                                                             │
│  ┌──────────┐  ┌──────────────┐  ┌──────────────────┐      │
│  │bridge-pty│◄►│ bridge-core  │◄►│   bridge-slack    │      │
│  │ ConPTY / │  │  traits +    │  │ HTTP API client + │      │
│  │  Unix PTY│  │  url/cmd/    │  │  TUI renderer +   │      │
│  │          │  │  types       │  │  rate limiter     │      │
│  └──────────┘  └──────┬───────┘  └──────────────────┘      │
│                       │                                     │
│                ┌──────▼──────┐                              │
│                │ bridge-auth │                              │
│                │ wry/tao     │                              │
│                │ browser flow│                              │
│                │ + storage   │                              │
│                └─────────────┘                              │
│                                                             │
│  attach/  protocol + server + client + per-OS launcher      │
│           (the local terminal mirror lives here)            │
└────────────────────────────────────────────────────────────┘
```

### Crates

| Crate | Purpose |
|-------|---------|
| `bridge-core` | `TerminalBackend` / `MessagingClient` traits, command parsing, URL helpers, shared types |
| `bridge-pty` | `portable-pty` wrapper: ConPTY on Windows, Unix PTY on macOS/Linux |
| `bridge-auth` | Browser-based Slack login (wry + tao) and credential storage |
| `bridge-slack` | Slack HTTP client (chat.postMessage, chat.update, conversations.history, users.conversations) + TUI renderer + rate limiter |
| `cli-bridge` | Binary: wires the crates together, owns the supervising loop and the local-attach server/client |

### Adding a new messaging client

1. Create `crates/bridge-teams/`
2. Implement the `MessagingClient` trait from `bridge-core`
3. Wire it into `cli-bridge/src/main.rs` alongside `SlackClient`

The `MessagingClient` trait requires `connect`/`disconnect`,
`send_message`/`edit_message`, and `subscribe`.

## Development

```
# Tests
cargo test

# Format check
cargo fmt --check

# Lint (workspace)
cargo clippy -p bridge-slack -p cli-bridge -p bridge-pty -p bridge-core --tests -- -D warnings

# Replay a captured PTY log through the renderer (debug rendering issues)
cargo run -p bridge-slack --example pty_replay -- pty.bin 120 24
```

CI runs build + test on Windows / macOS / Linux on every push and PR.

## License

Apache-2.0 — see [LICENSE](LICENSE).
