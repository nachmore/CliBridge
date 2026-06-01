# CliBridge

Bridge your local CLI to Slack. Interact with your terminal from anywhere — type commands in Slack, see output streamed back in real-time, with full TUI support via message edits.

## Features

- **Bidirectional terminal access** — Send commands from Slack, see output streamed back
- **TUI support** — Full-screen apps (vim, htop, tmux) render via message edits at 1 update/sec
- **Local terminal mirror** — Auto-spawns a real local terminal that mirrors the bridge; type locally, watch from Slack
- **Special commands** — Send Ctrl+C, resize, arrow keys, tmux prefix, slash commands, and more from Slack
- **Browser-based login** — Embedded WebView captures your Slack token + cookies (no bot setup needed)
- **Modular architecture** — Platform (Windows/macOS/Linux) and client (Slack/future: Teams) abstractions
- **Cross-platform** — Windows (ConPTY), macOS (Unix PTY), and Linux (Unix PTY)

## Quick Start

### 1. Extract your Slack token

```bash
cli-bridge --extract-tokens
```

This reads your `xoxc-` token and `d` cookie from the Slack desktop app's local storage. **Slack must be closed** while running this command.

### 2. Run the bridge

```bash
cli-bridge --workspace "My Workspace" --channel C0123456789
```

### 3. Interact from Slack

Type in the configured channel. Your messages are sent as terminal input. Output streams back as code-formatted messages.

## Installation

### From releases

Download the latest binary from [GitHub Releases](https://github.com/nachmore/CliBridge/releases).

### From source

```bash
git clone https://github.com/nachmore/CliBridge.git
cd CliBridge
cargo build --release
```

Binary will be at `target/release/cli-bridge` (or `.exe` on Windows).

## Configuration

CliBridge looks for config in this order:
1. `--config <path>` CLI argument
2. `./cli-bridge.toml` (current directory)
3. `~/.config/cli-bridge/config.toml` (user config dir)

See [`cli-bridge.example.toml`](cli-bridge.example.toml) for all options.

### Environment variables

| Variable | Description |
|----------|-------------|
| `CLI_BRIDGE_TOKEN` | Slack token (alternative to --extract-tokens) |
| `CLI_BRIDGE_COOKIE` | Slack `d` cookie value |
| `RUST_LOG` | Log level (e.g., `info`, `debug`, `trace`) |

## CLI Usage

```
cli-bridge [OPTIONS]

Options:
  -c, --channel <CHANNEL>      Slack channel ID
  -s, --shell <SHELL>          Shell to spawn (default: platform shell)
  -w, --workspace <WORKSPACE>  Workspace name or URL
      --url <URL>              Slack API base URL (for enterprise grids)
      --cols <COLS>            Terminal width in columns (default: 120)
      --rows <ROWS>            Terminal height in rows (default: 24)
      --no-local               Skip auto-opening a local terminal mirror
      --anchor-refresh <N>     Re-anchor the live TUI message every N inbound Slack messages (default: 10, 0 to disable)
      --name <NAME>            Display name for this session (used in banners and the local terminal title)
      --config <CONFIG>        Path to config file
      --login                  Open browser to sign in to Slack
      --list-workspaces        List saved workspaces
  -h, --help                   Print help
```

## Local terminal mirror

By default the bridge auto-opens a fresh terminal window that mirrors the shell:
keystrokes go to the same shell that's bridged to Slack, and shell output is
visible both locally and in Slack. Ctrl+C in the mirror window kills the
foreground shell job (as you'd expect); Ctrl+C in the bridge window quits the
bridge entirely. Pass `--no-local` to disable.

When the shell exits (you type `exit`, the process dies, etc.) the local
mirror window closes automatically and the bridge waits for `--new` from
Slack to start a new shell. Ctrl+C on the bridge while idle quits.

## Special Commands

When typing in Slack, prefix with `--` for special commands. (We avoid `/` because Slack treats those as native slash commands and never delivers them.)

| Command | Action |
|---------|--------|
| `--ctrl+c` | Send interrupt (SIGINT) |
| `--ctrl+d` | Send EOF |
| `--ctrl+z` | Suspend (SIGTSTP) |
| `--ctrl+l` | Clear screen |
| `--ctrl+\` | Send SIGQUIT |
| `--kill` | Kill the shell process |
| `--restart` / `--new` | (Re)spawn the shell. While a shell is alive, asks for confirmation; reply `--new force` to terminate and start fresh. After the shell has exited, plain `--new` works. |
| `--resize 120x40` | Resize terminal |
| `--clear` | Re-anchor: end the current edited message and start a new one on the next output |
| `--tab` | Send Tab key |
| `--esc` | Send Escape key |
| `--up` `--down` `--left` `--right` | Arrow keys |
| `--tmux <key>` | Send tmux prefix (Ctrl+B) + key |
| `--raw <hex>` | Send raw bytes (hex-encoded) |
| `--slash <name>` | Send a literal `/name` to the shell (e.g. `--slash init` for Claude Code) |
| `--name <text>` | Rename the session (shows up in start / exit / restart banners) |
| `--help` | Show command help |

Any other text is sent directly as terminal input with a newline appended.

## Architecture

```
┌─────────────────────────────────────────────────────┐
│                    cli-bridge                         │
│                  (binary crate)                       │
├─────────────────────────────────────────────────────┤
│                                                      │
│  ┌──────────┐    ┌──────────────┐    ┌───────────┐  │
│  │bridge-pty│◄──►│  bridge-core │◄──►│bridge-slack│  │
│  │(ConPTY/  │    │  (traits &   │    │(HTTP API + │  │
│  │ Unix PTY)│    │   commands)  │    │ TUI render)│  │
│  └──────────┘    └──────────────┘    └───────────┘  │
│                         ▲                            │
│                         │                            │
│                  ┌──────┴──────┐                     │
│                  │ bridge-auth │                     │
│                  │(LevelDB +   │                     │
│                  │ credential  │                     │
│                  │  storage)   │                     │
│                  └─────────────┘                     │
└─────────────────────────────────────────────────────┘
```

### Crates

| Crate | Purpose |
|-------|---------|
| `bridge-core` | Trait definitions (`TerminalBackend`, `MessagingClient`), command parsing, shared types |
| `bridge-pty` | PTY implementation using `portable-pty` (ConPTY on Windows, Unix PTY on macOS) |
| `bridge-auth` | Slack token extraction from LevelDB + encrypted credential storage |
| `bridge-slack` | Slack API client (chat.postMessage, chat.update) + TUI renderer + rate limiter |
| `cli-bridge` | Binary that wires everything together |

### Adding a new messaging client (e.g., Teams)

1. Create `crates/bridge-teams/`
2. Implement the `MessagingClient` trait from `bridge-core`
3. Wire it into `cli-bridge/src/main.rs` as an alternative to `SlackClient`

The `MessagingClient` trait requires:
- `connect()` / `disconnect()`
- `send_message()` / `edit_message()`
- `subscribe()` (returns a channel of incoming messages)

### TUI Rendering Strategy

The renderer detects full-screen applications by looking for ANSI escape sequences (alternate screen buffer, cursor positioning, screen clears). When detected:

1. Maintains a virtual screen buffer matching terminal dimensions
2. Processes ANSI escape sequences to update the buffer
3. Renders the buffer as a Slack code block
4. Edits the previous message (instead of posting new ones)
5. Throttled to 1 update/second (Slack's rate limit)

For regular streaming output, new messages are posted with content wrapped in code blocks.

## Development

```bash
# Build (fast debug)
cargo build

# Run tests
cargo test

# Run with logging
RUST_LOG=debug cargo run -- --channel C123 --workspace "My WS"

# Check formatting
cargo fmt --check

# Lint
cargo clippy -- -D warnings
```

## CI/CD

- **CI** (`ci.yml`): Runs on every push to main and PRs. Builds + tests on Windows, macOS ARM, and Linux. Uploads debug artifacts. Auto-dispatches nightly release on green.
- **Release** (`release.yml`): Stable releases via `v*.*.*` tags. Nightly releases auto-rotated (keeps latest + previous).

## License

MIT — see [LICENSE](LICENSE).
