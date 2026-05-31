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

    /// Workspace name or URL to connect to
    #[arg(short, long)]
    workspace: Option<String>,

    /// Path to config file
    #[arg(long)]
    config: Option<String>,

    /// Extract tokens from Slack desktop app and save them
    #[arg(long)]
    extract_tokens: bool,

    /// List saved workspaces
    #[arg(long)]
    list_workspaces: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    // Load config (file -> env -> CLI args, with CLI taking precedence)
    let config = AppConfig::load(cli.config.as_deref())?;

    if cli.extract_tokens {
        return commands::extract_tokens().await;
    }

    if cli.list_workspaces {
        return commands::list_workspaces();
    }

    // Determine final settings
    let channel = cli
        .channel
        .or(config.channel.clone())
        .expect("Channel is required. Use --channel or set it in config.");

    let shell = cli
        .shell
        .or(config.shell.clone())
        .unwrap_or_else(default_shell);

    let workspace = cli.workspace.or(config.workspace.clone());

    // Run the bridge
    bridge::run(&channel, &shell, workspace.as_deref()).await
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
    use bridge_auth::CredentialStore;
    use bridge_slack::SlackTokenExtractor;
    use tracing::info;

    pub async fn extract_tokens() -> Result<()> {
        println!("Extracting tokens from Slack desktop app...");
        println!("(Make sure Slack is closed)");

        let credentials = SlackTokenExtractor::extract_all()?;
        let store = CredentialStore::new()?;

        for cred in &credentials {
            store.save(cred)?;
            println!(
                "  ✓ Saved token for: {}",
                cred.workspace_name.as_deref().unwrap_or("Unknown")
            );
        }

        info!(
            "Extracted and saved {} workspace token(s)",
            credentials.len()
        );
        println!("\nDone! You can now run cli-bridge with --workspace <name>");
        Ok(())
    }

    pub fn list_workspaces() -> Result<()> {
        let store = CredentialStore::new()?;
        let workspaces = store.list_workspaces()?;

        if workspaces.is_empty() {
            println!("No saved workspaces. Run with --extract-tokens first.");
        } else {
            println!("Saved workspaces:");
            for ws in workspaces {
                println!("  • {ws}");
            }
        }
        Ok(())
    }
}
