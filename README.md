# CliBridge

Bridge your local CLI to Slack. Interact with your terminal from anywhere — type commands in Slack, see output streamed back in real-time, with full TUI support via message edits.

## Features

- **Bidirectional terminal access** — Send commands from Slack, see output streamed back
- **TUI support** — Full-screen apps (vim, htop, tmux) render via message edits at 1 update/sec
- **Local access preserved** — The spawned terminal is also accessible locally
- **Special commands** — Send Ctrl+C, resize, arrow keys, tmux prefix, and more from Slack
- **Auto token extraction** — Extracts Slack credentials from the desktop app (no bot setup needed)
- **Modular architecture** — Platform (Windows/macOS) and client (Slack/future: Teams) abstractions
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
      --config <CONFIG>         Path to config file
      --extract-tokens          Extract tokens from Slack desktop app
      --list-workspaces         List saved workspaces
  -h, --help                   Print help
```

## Special Commands

When typing in Slack, prefix with `/` for special commands:

| Command | Action |
|---------|--------|
| `/ctrl+c` | Send interrupt (SIGINT) |
| `/ctrl+d` | Send EOF |
| `/ctrl+z` | Suspend (SIGTSTP) |
| `/ctrl+l` | Clear screen |
| `/ctrl+\` | Send SIGQUIT |
| `/kill` | Kill the shell process |
| `/restart` | Restart the shell |
| `/resize 120x40` | Resize terminal |
| `/clear` | Clear message history |
| `/tab` | Send Tab key |
| `/esc` | Send Escape key |
| `/up` `/down` `/left` `/right` | Arrow keys |
| `/tmux <key>` | Send tmux prefix (Ctrl+B) + key |
| `/raw <hex>` | Send raw bytes (hex-encoded) |
| `/help` | Show command help |

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
