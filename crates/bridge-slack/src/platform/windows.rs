use anyhow::{Result, bail};
use rusty_leveldb::LdbIterator;
use std::path::Path;

/// Extract the `d` cookie from Slack's local storage on Windows.
pub fn extract_cookie_impl(slack_dir: &Path) -> Result<String> {
    let cookies_path = slack_dir.join("Cookies");
    let network_cookies_path = slack_dir.join("Network").join("Cookies");

    let path = if network_cookies_path.exists() {
        network_cookies_path
    } else if cookies_path.exists() {
        cookies_path
    } else {
        bail!("Cookie file not found. Is Slack installed?");
    };

    let local_storage_dir = path.parent().unwrap().join("Local Storage").join("leveldb");

    if local_storage_dir.exists() {
        let opts = rusty_leveldb::Options::default();
        if let Ok(mut db) = rusty_leveldb::DB::open(&local_storage_dir, opts) {
            let mut iter = db
                .new_iter()
                .map_err(|e| anyhow::anyhow!("Iterator error: {e}"))?;
            while let Some((_key, value)) = iter.next() {
                let value_str = String::from_utf8_lossy(&value);
                if let Some(cookie) = extract_d_cookie_from_value(&value_str) {
                    return Ok(cookie);
                }
            }
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
