use anyhow::{Result, bail};
use std::path::Path;

/// Extract the `d` cookie from Slack's local storage on Linux.
/// Scans LevelDB files directly to avoid lock conflicts with a running Slack.
pub fn extract_cookie_impl(slack_dir: &Path) -> Result<String> {
    let local_storage_dir = slack_dir.join("Local Storage").join("leveldb");

    if !local_storage_dir.exists() {
        bail!("LevelDB directory not found. Is Slack installed?");
    }

    let entries = std::fs::read_dir(&local_storage_dir)?;
    for entry in entries.flatten() {
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext != "ldb" && ext != "log" {
            continue;
        }

        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(_) => continue,
        };

        let content = String::from_utf8_lossy(&data);
        if let Some(cookie) = extract_d_cookie_from_value(&content) {
            return Ok(cookie);
        }
    }

    bail!(
        "Could not extract cookie automatically. \
         Please provide it manually via CLI_BRIDGE_COOKIE environment variable.\n\
         To get your cookie: Open Slack in a browser, go to DevTools > Application > Cookies > \
         app.slack.com, and copy the 'd' cookie value."
    )
}

fn extract_d_cookie_from_value(value: &str) -> Option<String> {
    if let Some(start) = value.find("xoxd-") {
        let rest = &value[start..];
        let end = rest
            .find(|c: char| c == '"' || c == ';' || c == '\'' || c.is_whitespace())
            .unwrap_or(rest.len());
        let cookie = &rest[..end];
        if cookie.len() > 10 {
            return Some(cookie.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_d_cookie() {
        let value = r#"something "xoxd-abc123def456" other"#;
        let cookie = extract_d_cookie_from_value(value).unwrap();
        assert_eq!(cookie, "xoxd-abc123def456");
    }

    #[test]
    fn test_extract_d_cookie_not_found() {
        let value = "no cookie here";
        assert!(extract_d_cookie_from_value(value).is_none());
    }
}
