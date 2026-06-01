# CliBridge

Bridge your local CLI to Slack. Drive a real shell from a Slack channel — type
commands in Slack, watch output stream back, and run TUI apps (vim, htop,
Claude Code) inside an emulated screen that renders live in a code block.
Optionally also see the same shell in a fresh local terminal window that
mirrors everything bidirectionally.

## Features

- **Bidirectional shell over Slack** — type commands in a channel, see output
  streamed and re-rendered as it changes. Bridge messages are tagged with a
  🌉 prefix so they're easy to spot and never echo back into the shell.
- **TUI emulation** — full virtual screen with ANSI escape support
  (CSI movement, ECH/DCH/ICH/IL/DL, scroll regions, save/restore cursor,
  xterm-style pending wrap, OSC, DCS, bracketed paste, DECTCEM cursor
  visibility, SGR-7 inverse-video tracking for app-drawn cursors).
  The live frame edits a single Slack message, rate-limited to ~1/sec.
- **Local terminal mirror** — auto-spawns a real OS terminal that mirrors the
  bridge. Type locally too; both Slack and the local window show the same
  shell.
- **Two-level scroll buffer** — content that scrolls off the live frame is
  posted into the channel as a 📜 *Scroll buffer* message that fills as new
  rows arrive; once full it locks as 📚 *History* and a fresh scroll buffer
  opens. Each row lives in exactly one Slack message — no duplication, no
  data loss on long bursts.
- **Runtime config from Slack** — `--config <key> <value>` from the channel
  flips knobs (cursor visibility, scroll-buffer size, anchor-refresh cadence,
  block-char replacement, …) without restarting. `--help config` lists them.
- **Browser-based login** — embedded WebView captures your `xoxc-` token + `d`
  cookie. No bot setup, no admin approval. Works with enterprise grids.
- **Portable credentials** — `--export-login` writes a single file you can
  copy or pipe to another machine (handy for SSH boxes that can't open a
  browser); `--import-login` reads it back.
- **Channel by name OR ID** — pass `--channel general` and we look it up,
  or `--channel C0123456789` to skip the API roundtrip.
- **Cross-platform** — Windows (ConPTY), macOS (Unix PTY + Terminal.app
  launcher), Linux (Unix PTY + GNOME Terminal/konsole/alacritty/xterm).

## Quick Start

### 1. Sign in to Slack

```
cargo run -- --login
```

A native WebView window opens at `slack.com/signin` (WebView2 on Windows,
WKWebView on macOS, WebKitGTK on Linux). Sign in normally; the window
captures your token + cookies on the first authenticated API call and
saves them to `~/.config/cli-bridge/credentials.json` (or the platform
equivalent).

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

### Running on a remote box (SSH)

`--login` opens a real browser window via WebView, which won't work over
SSH. Sign in once on a machine with a display, then move the credentials:

```sh
# On your laptop (where you can run a browser):
cli-bridge --login
cli-bridge --export-login - | ssh devbox 'cli-bridge --import-login -'

# Now on devbox:
ssh devbox
cli-bridge --workspace acme --channel general --no-local
```

`--no-local` skips the local-terminal mirror, which doesn't make sense over
SSH anyway. The Slack channel becomes your only view of the shell.

If you'd rather not pipe over the wire:

```sh
cli-bridge --export-login slack-creds.json
scp slack-creds.json devbox:~/
ssh devbox 'cli-bridge --import-login slack-creds.json && rm slack-creds.json'
```

The export file holds your `xoxc-` token + `d` cookie — enough to act as
you in Slack indefinitely. **Treat it like a password**: don't commit it,
don't email it, delete it after import.

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
                               single-cell ASCII approximations (#, [,
                               ], ', ., :, /, \, _) in Slack output.
                               Slack's font fallback renders these
                               wider than one cell and pushes box-
                               drawing layouts (e.g. the Claude Code
                               banner) out of column. The local attach
                               window is unaffected. The cursor mark
                               itself stays as █ regardless.
      --hide-cursor            Don't render the cursor in the live frame.
                               By default the cursor cell is shown as █
                               so you can see where it sits when driving
                               the session via Slack (e.g. arrow-key
                               navigation in a line editor). Apps that
                               hide the OS cursor (Claude Code via
                               DECTCEM) are honored automatically; their
                               app-drawn cursors (inverse-video space)
                               surface as █ via SGR-7 tracking.

Commands:
      --login                  Open a browser to sign in to Slack and save
                               credentials.
      --list-workspaces        List saved workspaces.
      --export-login <PATH>    Export saved credentials for a workspace
                               to a single file you can copy to another
                               machine (e.g. for SSH use). Use `-` for
                               stdout. Pair with --workspace <name> to
                               disambiguate when multiple are saved.
                               File contains a long-lived token + auth
                               cookie — treat it like a password.
      --import-login <PATH>    Import credentials previously written by
                               --export-login. Use `-` for stdin (e.g.
                               `cat creds.json | cli-bridge --import-login -`).

Slack:
      --url <URL>              Slack API base URL (for enterprise grids
                               that don't auto-derive from the workspace
                               URL — usually unneeded).

Debug:
      --pty-log <PATH>         Capture every byte of PTY output to file for
                               offline replay via the pty_replay example.
      --config <PATH>          Path to a TOML config file. (Note: from
                               *inside* a Slack channel, `--config` means
                               something different — runtime setting
                               read/write — see Special Commands below.)
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
| `--enter` (aliases `--return`, `--cr`) | Send a bare Enter, no text |
| `--up` `--down` `--left` `--right` | Arrow keys |
| `--tmux <key>` | Send tmux prefix (Ctrl+B) + key |
| `--raw <hex>` | Send raw bytes (hex-encoded) |
| `--slash <name>` | Send a literal `/name` to the shell (e.g. `--slash init` for Claude Code) |
| `--name <text>` | Rename the session (shortcut for `--config name <text>`) |
| `--config` | List runtime-mutable settings + current values |
| `--config <key>` | Show one setting with its description |
| `--config <key> <value>` | Set one (e.g. `--config show_cursor off`) |
| `--help` | Show this help |
| `--help config` | List every configurable setting with descriptions |

Any other text is sent verbatim to the shell, wrapped in bracketed-paste
markers (`\x1b[200~ ... \x1b[201~`) with a trailing CR. The bracketed-paste
wrapping makes Enter work correctly with TUI editors like Claude Code that
keep paste mode permanently enabled — without it, the line ends up in the
editor's buffer but never submits.

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
1. `--config <path>` CLI argument (file selection — note the long form takes a path here)
2. `./cli-bridge.toml` (current directory)
3. `~/.config/cli-bridge/config.toml` (or platform equivalent)

See [`cli-bridge.example.toml`](cli-bridge.example.toml) for all options.

A subset of the same keys is **runtime-mutable from Slack** without
restarting the bridge. Send `--config` in the channel to list them, or
`--help config` for descriptions:

```
--config show_cursor off
--config anchor_refresh 5
--config scroll_buffer 5000
--config name "build server"
```

Currently runtime-mutable: `replace_block_chars`, `show_cursor`,
`anchor_refresh`, `scroll_buffer`, `name`.

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
| `bridge-core` | `TerminalBackend` / `MessagingClient` traits, command parser (incl. `--config`/`--help` topic dispatch), URL helpers, shared types |
| `bridge-pty` | `portable-pty` wrapper: ConPTY on Windows, Unix PTY on macOS/Linux |
| `bridge-auth` | Browser-based Slack login (wry + tao), credential storage, portable export/import (`LoginExport`) |
| `bridge-slack` | Slack HTTP client (chat.postMessage, chat.update, conversations.history, users.conversations + edge-search fallback for enterprise grids), TUI renderer (full CSI/SGR/DECTCEM/inverse-video tracking), two-level scroll buffer, rate limiter, self-echo defenses (🌉 prefix + ts ring + text ring) |
| `cli-bridge` | Binary: wires the crates together. Modules: `bridge` (supervising loop), `attach` (local-terminal protocol/server/client), `settings` (runtime `--config` registry), plus startup `config.rs` (TOML loader) |

### Adding a new messaging client

1. Create `crates/bridge-teams/`
2. Implement the `MessagingClient` trait from `bridge-core`
3. Wire it into `cli-bridge/src/main.rs` alongside `SlackClient`

The `MessagingClient` trait requires `connect`/`disconnect`,
`send_message`/`edit_message`, and `subscribe`.

## Development

```
# Tests
cargo test --workspace

# Format check
cargo fmt --check

# Lint (workspace)
cargo clippy --workspace --all-targets

# Replay a captured PTY log through the renderer (debug rendering issues)
cargo run -p bridge-slack --example pty_replay -- pty.bin 120 24
```

When debugging rendering bugs, the workflow that keeps paying off is:

1. Run with `--pty-log pty.bin` to capture the raw byte stream.
2. Reproduce the bug in Slack.
3. Inspect `pty.bin` to find the exact escape sequence Claude Code (or
   whatever app) emitted at the moment the bug appeared.
4. Add a regression test that feeds the offending bytes through `process()`.
5. Replay the captured log through the renderer offline via
   `cargo run -p bridge-slack --example pty_replay` to verify the fix.

Several existing tests originated this way; grep for `pty.bin` in commit
messages for examples.

CI runs build + test on Windows / macOS / Linux on every push and PR.

## License

Apache-2.0 — see [LICENSE](LICENSE).
