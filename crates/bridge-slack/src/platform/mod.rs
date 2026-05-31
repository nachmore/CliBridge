#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

use anyhow::Result;
use std::path::Path;

/// Extract the `d` cookie from Slack's local storage.
/// Dispatches to the platform-specific implementation.
pub fn extract_cookie(slack_dir: &Path) -> Result<String> {
    #[cfg(target_os = "windows")]
    {
        windows::extract_cookie_impl(slack_dir)
    }
    #[cfg(target_os = "macos")]
    {
        macos::extract_cookie_impl(slack_dir)
    }
    #[cfg(target_os = "linux")]
    {
        linux::extract_cookie_impl(slack_dir)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        let _ = slack_dir;
        anyhow::bail!("Cookie extraction is not supported on this platform.")
    }
}
