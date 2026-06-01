use std::fs;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Application configuration, loaded from TOML file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    /// Slack channel ID to use
    pub channel: Option<String>,
    /// Shell command to spawn
    pub shell: Option<String>,
    /// Workspace name or URL
    pub workspace: Option<String>,
    /// Terminal columns
    pub cols: Option<u16>,
    /// Terminal rows
    pub rows: Option<u16>,
    /// Minimum seconds between message updates (for TUI mode)
    pub update_interval_secs: Option<u64>,
    /// Re-anchor the live TUI message every N inbound Slack messages so it
    /// stays near the bottom of the channel as the user types. 0 disables.
    pub anchor_refresh: Option<u32>,
    /// Friendly display name for the session, used in lifecycle banners.
    pub name: Option<String>,
    /// Lines of scroll buffer to retain above the live TUI frame.
    /// 0 disables. Old key name `scrollback` is still accepted.
    #[serde(alias = "scrollback")]
    pub scroll_buffer: Option<usize>,
    /// Replace Unicode Block Elements with spaces when sending to Slack.
    /// Default: false. Avoids column drift in box-drawing layouts where
    /// Slack's font fallback renders block chars wider than one cell.
    pub replace_block_chars: Option<bool>,
    /// Show the terminal cursor as █ in the live frame. Default: true.
    /// Set false to hide it.
    pub show_cursor: Option<bool>,
}

impl AppConfig {
    /// Load config from file, falling back to default locations.
    pub fn load(path: Option<&str>) -> Result<Self> {
        if let Some(path) = path {
            let content = fs::read_to_string(path)?;
            let config: AppConfig = toml::from_str(&content)?;
            return Ok(config);
        }

        // Try default locations
        let default_paths = [
            "cli-bridge.toml",
            &format!(
                "{}/cli-bridge/config.toml",
                dirs::config_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            ),
        ];

        for path in &default_paths {
            if Path::new(path).exists() {
                let content = fs::read_to_string(path)?;
                let config: AppConfig = toml::from_str(&content)?;
                return Ok(config);
            }
        }

        Ok(AppConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = AppConfig::default();
        assert!(config.channel.is_none());
        assert!(config.shell.is_none());
    }

    #[test]
    fn test_parse_toml() {
        let toml_str = r#"
            channel = "C12345"
            shell = "bash"
            workspace = "my-workspace"
            cols = 120
            rows = 40
        "#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.channel.unwrap(), "C12345");
        assert_eq!(config.shell.unwrap(), "bash");
        assert_eq!(config.cols.unwrap(), 120);
    }
}
