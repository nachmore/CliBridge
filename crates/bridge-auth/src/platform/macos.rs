use anyhow::{Result, bail};
use std::path::Path;

/// Extract the `d` cookie from Slack's cookie store on macOS.
/// Not yet implemented — cookies are encrypted with a Keychain-derived key.
pub fn extract_cookie_impl(slack_dir: &Path) -> Result<String> {
    let cookies_path = slack_dir.join("Cookies");
    if !cookies_path.exists() {
        bail!("Cookie file not found at: {}", cookies_path.display());
    }

    bail!(
        "Automatic cookie extraction on macOS is not yet implemented. \
         Please provide it manually via CLI_BRIDGE_COOKIE environment variable.\n\
         To get your cookie: Open Slack in a browser, go to DevTools > Application > Cookies > \
         app.slack.com, and copy the 'd' cookie value."
    )
}
