mod bridge;
mod config;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::config::AppConfig;

#[derive(Parser)]
#[command(name = "cli-bridge", about = "Bridge your CLI to messaging platforms")]
struct Cli {
    /// Channel ID to use for communication
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

    /// Terminal height in rows (default: 40)
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
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    let config = AppConfig::load(cli.config.as_deref())?;

    if cli.login {
        return commands::login();
    }

    if cli.list_workspaces {
        return commands::list_workspaces();
    }

    let channel = cli
        .channel
        .or(config.channel.clone())
        .expect("Channel is required. Use --channel or set it in config.");

    let shell = cli
        .shell
        .or(config.shell.clone())
        .unwrap_or_else(default_shell);

    let workspace = cli.workspace.or(config.workspace.clone());

    // Terminal size: CLI > config > default. Default is 120x40 — wide enough
    // for most TUIs to lay out side-by-side panels without overflowing Slack's
    // code-block render.
    let size = bridge_core::types::TerminalSize {
        cols: cli.cols.or(config.cols).unwrap_or(120),
        rows: cli.rows.or(config.rows).unwrap_or(40),
    };

    bridge::run(
        &channel,
        &shell,
        workspace.as_deref(),
        cli.url.as_deref(),
        size,
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
    use anyhow::Result;
    use bridge_auth::{CredentialStore, login as browser_login};
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
