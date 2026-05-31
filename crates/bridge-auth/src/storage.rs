use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use bridge_core::types::Credentials;

/// Stores and retrieves credentials from a local config file.
pub struct CredentialStore {
    path: PathBuf,
}

#[derive(Serialize, Deserialize, Default)]
struct StoredCredentials {
    workspaces: Vec<StoredWorkspace>,
}

#[derive(Serialize, Deserialize, Clone)]
struct StoredWorkspace {
    name: String,
    url: String,
    token: String,
    cookie: String,
}

impl CredentialStore {
    pub fn new() -> Result<Self> {
        let config_dir = dirs::config_dir()
            .context("Could not determine config directory")?
            .join("cli-bridge");
        fs::create_dir_all(&config_dir)?;
        Ok(Self {
            path: config_dir.join("credentials.json"),
        })
    }

    /// Create a store at a specific path (for testing).
    pub fn with_path(path: PathBuf) -> Self {
        Self { path }
    }

    /// Save credentials for a workspace.
    pub fn save(&self, credentials: &Credentials) -> Result<()> {
        let mut stored = self.load_all()?;

        let workspace = StoredWorkspace {
            name: credentials.workspace_name.clone().unwrap_or_default(),
            url: credentials.workspace_url.clone().unwrap_or_default(),
            token: credentials.token.clone(),
            cookie: credentials.cookie.clone().unwrap_or_default(),
        };

        // Update existing or add new
        if let Some(existing) = stored
            .workspaces
            .iter_mut()
            .find(|w| w.url == workspace.url)
        {
            *existing = workspace;
        } else {
            stored.workspaces.push(workspace);
        }

        let json = serde_json::to_string_pretty(&stored)?;
        fs::write(&self.path, json)?;
        Ok(())
    }

    /// Load credentials for a specific workspace URL.
    pub fn load(&self, workspace_url: &str) -> Result<Option<Credentials>> {
        let stored = self.load_all()?;
        Ok(stored
            .workspaces
            .iter()
            .find(|w| w.url == workspace_url || w.name == workspace_url)
            .map(|w| Credentials {
                token: w.token.clone(),
                cookie: Some(w.cookie.clone()),
                workspace_url: Some(w.url.clone()),
                workspace_name: Some(w.name.clone()),
            }))
    }

    /// List all stored workspace names.
    pub fn list_workspaces(&self) -> Result<Vec<String>> {
        let stored = self.load_all()?;
        Ok(stored.workspaces.iter().map(|w| w.name.clone()).collect())
    }

    fn load_all(&self) -> Result<StoredCredentials> {
        if !self.path.exists() {
            return Ok(StoredCredentials::default());
        }
        let content = fs::read_to_string(&self.path)?;
        let stored: StoredCredentials = serde_json::from_str(&content)?;
        Ok(stored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_save_and_load() {
        let dir = TempDir::new().unwrap();
        let store = CredentialStore::with_path(dir.path().join("creds.json"));

        let creds = Credentials {
            token: "xoxc-test-token".to_string(),
            cookie: Some("xoxd-test-cookie".to_string()),
            workspace_url: Some("https://test.slack.com".to_string()),
            workspace_name: Some("Test Workspace".to_string()),
        };

        store.save(&creds).unwrap();

        let loaded = store.load("https://test.slack.com").unwrap().unwrap();
        assert_eq!(loaded.token, "xoxc-test-token");
        assert_eq!(loaded.cookie.unwrap(), "xoxd-test-cookie");
    }

    #[test]
    fn test_load_nonexistent() {
        let dir = TempDir::new().unwrap();
        let store = CredentialStore::with_path(dir.path().join("creds.json"));
        let result = store.load("https://nonexistent.slack.com").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_list_workspaces() {
        let dir = TempDir::new().unwrap();
        let store = CredentialStore::with_path(dir.path().join("creds.json"));

        let creds = Credentials {
            token: "xoxc-1".to_string(),
            cookie: Some("xoxd-1".to_string()),
            workspace_url: Some("https://ws1.slack.com".to_string()),
            workspace_name: Some("Workspace 1".to_string()),
        };
        store.save(&creds).unwrap();

        let workspaces = store.list_workspaces().unwrap();
        assert_eq!(workspaces, vec!["Workspace 1"]);
    }

    #[test]
    fn test_update_existing() {
        let dir = TempDir::new().unwrap();
        let store = CredentialStore::with_path(dir.path().join("creds.json"));

        let creds = Credentials {
            token: "xoxc-old".to_string(),
            cookie: Some("xoxd-old".to_string()),
            workspace_url: Some("https://test.slack.com".to_string()),
            workspace_name: Some("Test".to_string()),
        };
        store.save(&creds).unwrap();

        let updated = Credentials {
            token: "xoxc-new".to_string(),
            cookie: Some("xoxd-new".to_string()),
            workspace_url: Some("https://test.slack.com".to_string()),
            workspace_name: Some("Test".to_string()),
        };
        store.save(&updated).unwrap();

        let loaded = store.load("https://test.slack.com").unwrap().unwrap();
        assert_eq!(loaded.token, "xoxc-new");
    }
}
