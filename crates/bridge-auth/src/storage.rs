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

/// Versioned envelope for portable credential exports. We deliberately
/// give this its own filename / shape rather than dumping the raw
/// `credentials.json` so we can evolve the on-disk schema without
/// breaking exports made by older versions.
///
/// File contents are sensitive: they're the same xoxc token + d cookie
/// that grant full access to the user's Slack workspace. Treat them
/// like a password.
#[derive(Serialize, Deserialize)]
pub struct LoginExport {
    /// Schema version. Bump when the shape changes.
    pub version: u32,
    /// Workspace name (the same `--workspace <name>` arg used at runtime).
    pub workspace_name: String,
    /// Workspace origin URL (e.g. `https://acme.slack.com`).
    pub workspace_url: String,
    /// xoxc- session token.
    pub token: String,
    /// `d` cookie value.
    pub cookie: String,
}

const EXPORT_VERSION: u32 = 1;

impl CredentialStore {
    /// Build an export envelope for a single workspace. `workspace` can
    /// be either a saved name or URL — same lookup as `load`. Returns
    /// `Ok(None)` if no matching workspace is stored.
    pub fn export(&self, workspace: &str) -> Result<Option<LoginExport>> {
        let stored = self.load_all()?;
        Ok(stored
            .workspaces
            .iter()
            .find(|w| w.name == workspace || w.url == workspace)
            .map(|w| LoginExport {
                version: EXPORT_VERSION,
                workspace_name: w.name.clone(),
                workspace_url: w.url.clone(),
                token: w.token.clone(),
                cookie: w.cookie.clone(),
            }))
    }

    /// Save credentials from an import envelope. Replaces the existing
    /// workspace with the same URL if there is one, otherwise appends.
    /// Returns the workspace name actually stored.
    pub fn import(&self, export: &LoginExport) -> Result<String> {
        if export.version != EXPORT_VERSION {
            anyhow::bail!(
                "Unsupported login export version {} (this binary expects {}). \
                 Try the latest cli-bridge or re-export from a compatible version.",
                export.version,
                EXPORT_VERSION
            );
        }
        let creds = Credentials {
            token: export.token.clone(),
            cookie: Some(export.cookie.clone()),
            workspace_url: Some(export.workspace_url.clone()),
            workspace_name: Some(export.workspace_name.clone()),
        };
        self.save(&creds)?;
        Ok(export.workspace_name.clone())
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
    fn test_export_import_roundtrip() {
        let dir = TempDir::new().unwrap();
        let store = CredentialStore::with_path(dir.path().join("creds.json"));

        let creds = Credentials {
            token: "xoxc-export-test".to_string(),
            cookie: Some("xoxd-export-test".to_string()),
            workspace_url: Some("https://example.slack.com".to_string()),
            workspace_name: Some("example".to_string()),
        };
        store.save(&creds).unwrap();

        // Export by name.
        let export = store.export("example").unwrap().unwrap();
        assert_eq!(export.version, EXPORT_VERSION);
        assert_eq!(export.token, "xoxc-export-test");
        assert_eq!(export.cookie, "xoxd-export-test");
        assert_eq!(export.workspace_url, "https://example.slack.com");

        // Round-trip via JSON to mimic what gets written/read on disk.
        let json = serde_json::to_string(&export).unwrap();
        let parsed: LoginExport = serde_json::from_str(&json).unwrap();

        // Import into a fresh store.
        let dir2 = TempDir::new().unwrap();
        let store2 = CredentialStore::with_path(dir2.path().join("creds.json"));
        let name = store2.import(&parsed).unwrap();
        assert_eq!(name, "example");

        let loaded = store2.load("example").unwrap().unwrap();
        assert_eq!(loaded.token, "xoxc-export-test");
        assert_eq!(loaded.cookie.as_deref(), Some("xoxd-export-test"));
    }

    #[test]
    fn test_export_unknown_workspace_returns_none() {
        let dir = TempDir::new().unwrap();
        let store = CredentialStore::with_path(dir.path().join("creds.json"));
        assert!(store.export("nope").unwrap().is_none());
    }

    #[test]
    fn test_import_rejects_wrong_version() {
        let dir = TempDir::new().unwrap();
        let store = CredentialStore::with_path(dir.path().join("creds.json"));
        let bad = LoginExport {
            version: 999,
            workspace_name: "x".to_string(),
            workspace_url: "https://x.slack.com".to_string(),
            token: "t".to_string(),
            cookie: "c".to_string(),
        };
        let err = store.import(&bad).unwrap_err();
        assert!(err.to_string().contains("Unsupported login export version"));
    }

    #[test]
    fn test_export_by_url() {
        let dir = TempDir::new().unwrap();
        let store = CredentialStore::with_path(dir.path().join("creds.json"));
        let creds = Credentials {
            token: "xoxc-1".to_string(),
            cookie: Some("xoxd-1".to_string()),
            workspace_url: Some("https://by-url.slack.com".to_string()),
            workspace_name: Some("byurl".to_string()),
        };
        store.save(&creds).unwrap();
        // Looking up by URL should also work.
        let export = store.export("https://by-url.slack.com").unwrap().unwrap();
        assert_eq!(export.workspace_name, "byurl");
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
