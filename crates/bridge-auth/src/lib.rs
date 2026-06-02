mod storage;

pub use storage::{CredentialStore, LoginExport};

#[cfg(feature = "browser-login")]
mod browser_login;

#[cfg(feature = "browser-login")]
pub use browser_login::login;

/// Stub for builds without the `browser-login` feature (e.g. headless Linux,
/// where WebKitGTK isn't available). Interactive login isn't possible there;
/// the user transfers credentials with `--export-login` on a desktop machine
/// and `--import-login` on the headless host.
#[cfg(not(feature = "browser-login"))]
pub fn login() -> anyhow::Result<bridge_core::types::Credentials> {
    anyhow::bail!(
        "This build has no embedded browser login (compiled without the \
         `browser-login` feature). Run `--login` on a machine with a desktop \
         browser, then `--export-login` there and `--import-login` here."
    )
}
