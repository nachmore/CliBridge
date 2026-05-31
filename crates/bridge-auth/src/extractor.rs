use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusty_leveldb::LdbIterator;
use tracing::{debug, info};

use bridge_core::types::Credentials;

use crate::platform;

/// Extracts Slack tokens from the desktop app's local storage.
///
/// The Slack desktop app stores:
/// - User tokens (xoxc-) in a LevelDB database (localStorage)
/// - Session cookie (d/xoxd-) in an encrypted cookie store
///
/// This extractor reads both to provide full API access.
pub struct SlackTokenExtractor;

impl SlackTokenExtractor {
    /// Extract all workspace tokens and the session cookie from the Slack desktop app.
    pub fn extract_all() -> Result<Vec<Credentials>> {
        let slack_dir = Self::find_slack_data_dir()?;
        info!("Found Slack data directory: {}", slack_dir.display());

        let tokens = Self::extract_tokens_from_leveldb(&slack_dir)?;
        let cookie = platform::extract_cookie(&slack_dir)?;

        Ok(tokens
            .into_iter()
            .map(|t| Credentials {
                token: t.token,
                cookie: Some(cookie.clone()),
                workspace_url: Some(t.url),
                workspace_name: Some(t.name),
            })
            .collect())
    }

    /// Find the Slack desktop app's data directory.
    fn find_slack_data_dir() -> Result<PathBuf> {
        let base = if cfg!(windows) {
            dirs::data_dir()
                .context("Could not find AppData directory")?
                .join("Slack")
        } else if cfg!(target_os = "macos") {
            dirs::home_dir()
                .context("Could not find home directory")?
                .join("Library/Application Support/Slack")
        } else {
            dirs::config_dir()
                .context("Could not find config directory")?
                .join("Slack")
        };

        if !base.exists() {
            bail!(
                "Slack data directory not found at: {}. Is Slack desktop installed?",
                base.display()
            );
        }

        Ok(base)
    }

    /// Extract tokens from Slack's LevelDB localStorage.
    fn extract_tokens_from_leveldb(slack_dir: &Path) -> Result<Vec<WorkspaceToken>> {
        let local_storage_dir = slack_dir.join("Local Storage").join("leveldb");

        if !local_storage_dir.exists() {
            bail!(
                "LevelDB directory not found at: {}. Make sure Slack is closed.",
                local_storage_dir.display()
            );
        }

        debug!("Reading LevelDB at: {}", local_storage_dir.display());

        let opts = rusty_leveldb::Options::default();
        let mut db = rusty_leveldb::DB::open(&local_storage_dir, opts)
            .map_err(|e| anyhow::anyhow!("Failed to open LevelDB: {e}. Is Slack closed?"))?;

        let mut tokens = Vec::new();

        let mut iter = db
            .new_iter()
            .map_err(|e| anyhow::anyhow!("Failed to create iterator: {e}"))?;

        while let Some((key, value)) = iter.next() {
            let key_str = String::from_utf8_lossy(&key);
            let value_str = String::from_utf8_lossy(&value);

            if key_str.contains("localConfig_v2")
                && let Some(token_info) = Self::parse_local_config(&value_str)
            {
                tokens.push(token_info);
            }
        }

        if tokens.is_empty() {
            bail!("No Slack tokens found in LevelDB. Make sure you're logged into Slack.");
        }

        info!("Found {} workspace token(s)", tokens.len());
        Ok(tokens)
    }

    /// Parse the localConfig_v2 JSON to extract token, workspace name, and URL.
    fn parse_local_config(value: &str) -> Option<WorkspaceToken> {
        let json_start = value.find('{')?;
        let json_str = &value[json_start..];

        let parsed: serde_json::Value = serde_json::from_str(json_str).ok()?;

        let teams = parsed.get("teams").or_else(|| parsed.get("workspaces"))?;

        if let Some(teams_obj) = teams.as_object() {
            for (_id, team) in teams_obj {
                let token = team.get("token")?.as_str()?;
                if !token.starts_with("xoxc-") {
                    continue;
                }
                let name = team
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("Unknown");
                let url = team.get("url").and_then(|u| u.as_str()).unwrap_or("");

                return Some(WorkspaceToken {
                    token: token.to_string(),
                    name: name.to_string(),
                    url: url.to_string(),
                });
            }
        }

        None
    }
}

struct WorkspaceToken {
    token: String,
    name: String,
    url: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_local_config_with_teams() {
        let json = r#"{"teams":{"T12345":{"token":"xoxc-test-token-123","name":"My Workspace","url":"https://myworkspace.slack.com"}}}"#;
        let result = SlackTokenExtractor::parse_local_config(json).unwrap();
        assert_eq!(result.token, "xoxc-test-token-123");
        assert_eq!(result.name, "My Workspace");
        assert_eq!(result.url, "https://myworkspace.slack.com");
    }

    #[test]
    fn test_parse_local_config_with_prefix() {
        let json = "\x01{\"teams\":{\"T12345\":{\"token\":\"xoxc-abc\",\"name\":\"WS\",\"url\":\"https://ws.slack.com\"}}}";
        let result = SlackTokenExtractor::parse_local_config(json).unwrap();
        assert_eq!(result.token, "xoxc-abc");
    }

    #[test]
    fn test_parse_local_config_no_xoxc() {
        let json = r#"{"teams":{"T12345":{"token":"xoxb-bot-token","name":"Bot","url":"https://bot.slack.com"}}}"#;
        let result = SlackTokenExtractor::parse_local_config(json);
        assert!(result.is_none());
    }
}
