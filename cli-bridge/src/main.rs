mod attach;
mod bridge;
mod config;
mod settings;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::config::AppConfig;

#[derive(Parser)]
#[command(name = "cli-bridge", about = "Bridge your CLI to messaging platforms")]
struct Cli {
    /// Slack channel — either an ID (e.g. `C0123456789`, `G017KTQLT5M`,
    /// `D01ABCDEFGH`) or a name (e.g. `general` or `#general`). IDs are
    /// used verbatim; names are resolved via the Slack API at startup.
    /// The heuristic: starts with C/G/D and is otherwise uppercase
    /// alphanumeric → treated as an ID; anything else → treated as a name.
    #[arg(short, long)]
    channel: Option<String>,

    /// Shell command to spawn (default: platform shell)
    #[arg(short, long)]
    shell: Option<String>,

    /// Workspace name or URL (selects credentials saved by `--login`)
    #[arg(short, long)]
    workspace: Option<String>,

    /// Slack API base URL (default: https://slack.com/api, enterprise: https://myco.enterprise.slack.com/api)
    #[arg(long)]
    url: Option<String>,

    /// Terminal width in columns (default: 120). Wider gives TUI apps room
    /// to lay out boxes side-by-side; narrower keeps Slack from line-wrapping.
    #[arg(long)]
    cols: Option<u16>,

    /// Terminal height in rows (default: 24). Bumping this just makes the
    /// rendered TUI frame taller in Slack, since trailing-whitespace rows are
    /// trimmed — width is what gets you nicer layouts.
    #[arg(long)]
    rows: Option<u16>,

    /// Path to config file
    #[arg(long)]
    config: Option<String>,

    /// Open a browser window to sign in to Slack and save credentials
    #[arg(long)]
    login: bool,

    /// List saved workspaces
    #[arg(long)]
    list_workspaces: bool,

    /// Export a workspace's saved credentials (token + d cookie) to a
    /// portable file you can copy to another machine — useful for
    /// running cli-bridge over SSH on a host that can't open a browser
    /// for `--login`. Pass a path, or `-` for stdout. Pair with
    /// `--workspace <name>` to choose which workspace to export when
    /// you have more than one saved.
    ///
    /// **Treat the resulting file like a password.** It contains the
    /// same xoxc token + d cookie that grant full access to your
    /// Slack workspace.
    #[arg(long, value_name = "PATH")]
    export_login: Option<String>,

    /// Import credentials previously written by `--export-login`. Pass
    /// a path, or `-` for stdin. The workspace name comes from the file.
    #[arg(long, value_name = "PATH")]
    import_login: Option<String>,

    /// Run as an attach client connected to a running bridge. The bridge
    /// process passes this when it auto-spawns the local terminal window —
    /// users normally don't pass it directly.
    #[arg(long, hide = true)]
    attach: Option<String>,

    /// Per-session shared token; required with --attach.
    #[arg(long, hide = true)]
    attach_token: Option<String>,

    /// Don't auto-open a local terminal window; only bridge to Slack.
    #[arg(long)]
    no_local: bool,

    /// Re-anchor the live TUI message every N inbound Slack messages, so it
    /// stays near the bottom of the channel as you type. 0 disables.
    /// Default: 10.
    #[arg(long)]
    anchor_refresh: Option<u32>,

    /// Display name for this session. Shows up in start / exit / restart
    /// banners. Can be changed at runtime with `--name <text>` from Slack.
    #[arg(long)]
    name: Option<String>,

    /// Capture every byte of PTY output to this file, verbatim. Useful for
    /// debugging rendering issues — capture a reproduction, then replay it
    /// through the renderer in a unit test. The file is overwritten each run.
    #[arg(long)]
    pty_log: Option<String>,

    /// How many lines of scroll buffer to retain above the live TUI frame
    /// in Slack messages. Lets you read content that scrolled off the top
    /// before the next anchor. 0 disables. Default: 10000.
    #[arg(long, alias = "scrollback")]
    scroll_buffer: Option<usize>,

    /// Replace Unicode Block Elements (U+2580–U+259F: █ ▌ ▐ ▛ etc.) with
    /// spaces in messages sent to Slack. Slack's code-block font lacks
    /// glyphs for these and falls back to a wider font, which pushes
    /// box-drawing edges out of column (e.g. the Claude Code welcome
    /// banner). The local attach window is unaffected. Off by default.
    #[arg(long)]
    replace_block_chars: bool,

    /// Hide the cursor overlay in the live frame. By default the cursor
    /// cell is rendered as █ so you can see where the cursor is when
    /// driving the session via Slack (e.g. arrow-key navigation in a
    /// line editor). Pass this flag to suppress it.
    #[arg(long)]
    hide_cursor: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    // Attach mode: run as a thin client over the wire protocol. Returns when
    // the bridge or the user closes the connection. We bypass config loading
    // for this path because it's a sub-invocation of cli-bridge itself.
    if let Some(addr) = cli.attach.as_deref() {
        let token = cli
            .attach_token
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--attach requires --attach-token"))?;
        return attach::run_attach_client(addr, token).await;
    }

    let config = AppConfig::load(cli.config.as_deref())?;

    if cli.login {
        return commands::login();
    }

    if cli.list_workspaces {
        return commands::list_workspaces();
    }

    if let Some(path) = cli.export_login.as_deref() {
        return commands::export_login(path, cli.workspace.as_deref());
    }

    if let Some(path) = cli.import_login.as_deref() {
        return commands::import_login(path);
    }

    // `--channel` (or `channel` in TOML) accepts either a Slack ID
    // (`C0123456789`) or a channel name (`general`). The bridge's
    // `looks_like_channel_id` heuristic discriminates and the API is only
    // hit when a name is given.
    let channel_or_name = cli.channel.or(config.channel.clone()).ok_or_else(|| {
        anyhow::anyhow!(
            "Channel is required. Use --channel <id-or-name> or set `channel` in config."
        )
    })?;

    let shell = cli
        .shell
        .or(config.shell.clone())
        .unwrap_or_else(default_shell);

    let workspace = cli.workspace.or(config.workspace.clone());

    // Terminal size: CLI > config > default. 120 cols gives TUIs room to lay
    // out side-by-side panels without overflowing Slack's code-block render;
    // 24 rows keeps the rendered frame about as tall as a normal terminal.
    let size = bridge_core::types::TerminalSize {
        cols: cli.cols.or(config.cols).unwrap_or(120),
        rows: cli.rows.or(config.rows).unwrap_or(24),
    };

    let anchor_refresh = cli.anchor_refresh.or(config.anchor_refresh).unwrap_or(10);
    let name = cli
        .name
        .or(config.name.clone())
        .unwrap_or_else(|| "CliBridge".to_string());
    let scroll_buffer = cli
        .scroll_buffer
        .or(config.scroll_buffer)
        .unwrap_or(bridge_slack::DEFAULT_SCROLL_BUFFER_LINES);
    let replace_block_chars = cli.replace_block_chars || config.replace_block_chars.unwrap_or(false);
    // CLI is opt-out; config key is opt-in (show_cursor: false). Either
    // route to "off" wins.
    let show_cursor = !cli.hide_cursor && config.show_cursor.unwrap_or(true);

    bridge::run(
        &channel_or_name,
        &shell,
        workspace.as_deref(),
        cli.url.as_deref(),
        size,
        !cli.no_local,
        anchor_refresh,
        name,
        cli.pty_log,
        scroll_buffer,
        replace_block_chars,
        show_cursor,
    )
    .await
}

fn default_shell() -> String {
    if cfg!(windows) {
        "cmd.exe".to_string()
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    }
}

mod commands {
    use std::io::{Read, Write};

    use anyhow::{Context, Result};
    use bridge_auth::{CredentialStore, LoginExport, login as browser_login};
    use tracing::info;

    pub fn login() -> Result<()> {
        println!("Opening Slack login window…");
        println!(
            "Sign in to your workspace; the window will close automatically once credentials are captured."
        );

        let mut credentials = browser_login()?;

        // The webview can't tell us a friendly workspace name, so let the user
        // pick one (this is what `--workspace <name>` will look up later).
        if credentials.workspace_name.is_none() {
            let derived = credentials
                .workspace_url
                .as_deref()
                .and_then(workspace_name_from_url)
                .unwrap_or_else(|| "default".to_string());
            credentials.workspace_name = Some(derived);
        }

        let store = CredentialStore::new()?;
        store.save(&credentials)?;

        let name = credentials.workspace_name.as_deref().unwrap_or("default");
        let url = credentials.workspace_url.as_deref().unwrap_or("(none)");
        println!("\n✓ Saved credentials for workspace '{name}' ({url})");
        println!("Run: cli-bridge --workspace \"{name}\" --channel <CHANNEL_ID>");
        info!("Login complete for workspace '{name}'");
        Ok(())
    }

    pub fn list_workspaces() -> Result<()> {
        let store = CredentialStore::new()?;
        let workspaces = store.list_workspaces()?;

        if workspaces.is_empty() {
            println!("No saved workspaces. Run with --login first.");
        } else {
            println!("Saved workspaces:");
            for ws in workspaces {
                println!("  • {ws}");
            }
        }
        Ok(())
    }

    /// Export the user's saved credentials for a single workspace as
    /// a portable JSON file.
    ///
    /// `workspace` is optional: if omitted and the user has exactly one
    /// saved workspace, we pick it; if there are multiple, we error
    /// out so the user has to be explicit.
    ///
    /// `path` is `-` for stdout (handy for piping over SSH) or a real
    /// filesystem path. On non-Windows targets the file is created with
    /// 0o600 permissions because the export contains the equivalent of
    /// a long-lived password to the user's Slack workspace.
    pub fn export_login(path: &str, workspace: Option<&str>) -> Result<()> {
        let store = CredentialStore::new()?;

        // Resolve which workspace to export.
        let target = match workspace {
            Some(w) => w.to_string(),
            None => {
                let names = store.list_workspaces()?;
                match names.len() {
                    0 => anyhow::bail!(
                        "No saved workspaces to export. Run --login first."
                    ),
                    1 => names.into_iter().next().unwrap(),
                    _ => anyhow::bail!(
                        "Multiple saved workspaces ({}). Pass --workspace <name> \
                         to choose which to export.",
                        names.join(", ")
                    ),
                }
            }
        };

        let export = store.export(&target)?.with_context(|| {
            format!("No saved credentials for workspace '{target}'")
        })?;
        let json = serde_json::to_string_pretty(&export)?;

        if path == "-" {
            // Pipe to stdout; the caller is responsible for keeping it
            // off-disk (typical pattern: `ssh remote cli-bridge --import-login -`).
            let stdout = std::io::stdout();
            let mut handle = stdout.lock();
            handle.write_all(json.as_bytes())?;
            handle.write_all(b"\n")?;
            // Don't print the security warning to stdout — it would
            // pollute the JSON. Send the warning to stderr.
            eprintln!(
                "⚠️ Credentials are sensitive: anyone with this file/stream can act \
                 as you in Slack. Don't store it in cleartext or in version control."
            );
        } else {
            write_export_file(path, json.as_bytes())?;
            eprintln!(
                "✓ Wrote credentials for workspace '{}' to {path}.",
                export.workspace_name
            );
            eprintln!(
                "⚠️ Treat this file like a password. Anyone who reads it can act \
                 as you in Slack. Delete it after importing on the target machine."
            );
        }
        info!("Exported login for workspace '{}'", export.workspace_name);
        Ok(())
    }

    /// Read an export file and store it in the local credential store.
    /// `path` is `-` for stdin or a filesystem path.
    pub fn import_login(path: &str) -> Result<()> {
        let json = if path == "-" {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            buf
        } else {
            std::fs::read_to_string(path)
                .with_context(|| format!("reading import file {path}"))?
        };

        let export: LoginExport = serde_json::from_str(&json)
            .context("parsing login export — was the file written by --export-login?")?;

        let store = CredentialStore::new()?;
        let name = store.import(&export)?;
        eprintln!("✓ Imported credentials for workspace '{name}'.");
        eprintln!(
            "Run: cli-bridge --workspace \"{name}\" --channel <channel-id-or-name>"
        );
        info!("Imported login for workspace '{name}'");
        Ok(())
    }

    /// Write an export to disk with restrictive permissions where the
    /// platform supports them. The export contains a long-lived
    /// session token + auth cookie; we don't want it to be
    /// world-readable on shared hosts.
    #[cfg(unix)]
    fn write_export_file(path: &str, contents: &[u8]) -> Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(contents)?;
        Ok(())
    }

    #[cfg(not(unix))]
    fn write_export_file(path: &str, contents: &[u8]) -> Result<()> {
        // Windows: there's no portable equivalent of 0o600 without
        // pulling in WinAPI to set DACLs. We rely on the user to put
        // the file in a sensible location (their own profile dir, not
        // a shared drive). The eprintln! warning at the call site is
        // the only mitigation here.
        std::fs::write(path, contents)?;
        Ok(())
    }

    /// Pull a friendly workspace identifier out of an origin URL.
    /// e.g. https://acme.enterprise.slack.com -> "acme"
    ///      https://acme.slack.com           -> "acme"
    ///      https://app.slack.com            -> None
    fn workspace_name_from_url(url: &str) -> Option<String> {
        let after_scheme = url.split("://").nth(1)?;
        let host = after_scheme.split('/').next()?;
        let first = host.split('.').next()?;
        if first == "app" || first == "slack" || first.is_empty() {
            None
        } else {
            Some(first.to_string())
        }
    }
}
