//! Open a new terminal window running `cli-bridge --attach <addr> --attach-token <tok>`.
//!
//! Each platform offers several terminal emulators; we try them in order of
//! "most likely to be present and behave nicely" and fall back to printing the
//! attach command if none work, so the user can run it manually.
//!
//! Why not detach the bridge process and have it spawn the *user's* shell into
//! a new terminal directly? Because the PTY it owns is a private pipe — there's
//! no way to attach a visible terminal window to an existing ConPTY/Unix PTY
//! after the fact. The split has to be: bridge process owns the PTY, separate
//! terminal-window process talks to it over our protocol.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

/// Attempt to open a new terminal window that runs the attach client.
/// Returns `Ok(())` if at least one launcher succeeded; `Err` if every launcher
/// failed, in which case the caller should print the attach command to the
/// user's existing console so they can run it themselves.
pub fn open_attach_terminal(addr: &str, token: &str) -> Result<()> {
    let exe = std::env::current_exe().context("Failed to locate cli-bridge executable")?;
    let exe_str = exe.to_string_lossy().into_owned();

    debug!(
        "Opening attach terminal for {} (exe: {})",
        addr,
        exe.display()
    );

    let attempts = launch_attempts(&exe_str, addr, token);
    let mut errors = Vec::new();
    for attempt in attempts {
        match attempt.try_spawn() {
            Ok(()) => {
                info!("Opened attach terminal via: {}", attempt.label);
                return Ok(());
            }
            Err(e) => {
                debug!("{} failed: {e}", attempt.label);
                errors.push(format!("{}: {e}", attempt.label));
            }
        }
    }
    anyhow::bail!(
        "Could not open a terminal window automatically. Tried: {}",
        errors.join("; ")
    )
}

struct LaunchAttempt {
    label: &'static str,
    program: PathBuf,
    args: Vec<String>,
}

impl LaunchAttempt {
    fn try_spawn(&self) -> Result<()> {
        let status = Command::new(&self.program)
            .args(&self.args)
            .spawn()
            .with_context(|| format!("spawn {}", self.program.display()))?;
        // We don't wait — the child process will outlive this call.
        let _ = status;
        Ok(())
    }
}

#[cfg(target_os = "windows")]
fn launch_attempts(exe: &str, addr: &str, token: &str) -> Vec<LaunchAttempt> {
    let attach_args = format!("--attach {addr} --attach-token {token}");
    vec![
        // Prefer Windows Terminal if installed — better font, resizable.
        LaunchAttempt {
            label: "Windows Terminal (wt.exe)",
            program: PathBuf::from("wt.exe"),
            args: vec![
                "new-tab".to_string(),
                "--title".to_string(),
                "CliBridge attach".to_string(),
                "cmd.exe".to_string(),
                "/c".to_string(),
                format!("\"{exe}\" {attach_args}"),
            ],
        },
        // Fallback: classic conhost via `cmd /c start`.
        LaunchAttempt {
            label: "cmd /c start",
            program: PathBuf::from("cmd.exe"),
            args: vec![
                "/c".to_string(),
                "start".to_string(),
                "\"CliBridge attach\"".to_string(),
                "/wait".to_string(),
                exe.to_string(),
                "--attach".to_string(),
                addr.to_string(),
                "--attach-token".to_string(),
                token.to_string(),
            ],
        },
    ]
}

#[cfg(target_os = "macos")]
fn launch_attempts(exe: &str, addr: &str, token: &str) -> Vec<LaunchAttempt> {
    // AppleScript: tell Terminal to open a new window running our attach command.
    // Quoting is fiddly — the script is one big string passed to osascript -e.
    let script = format!(
        r#"tell application "Terminal" to do script "'{exe}' --attach {addr} --attach-token {token}""#
    );
    vec![LaunchAttempt {
        label: "Terminal.app (osascript)",
        program: PathBuf::from("osascript"),
        args: vec!["-e".to_string(), script],
    }]
}

#[cfg(all(unix, not(target_os = "macos")))]
fn launch_attempts(exe: &str, addr: &str, token: &str) -> Vec<LaunchAttempt> {
    let cmd = format!("{exe} --attach {addr} --attach-token {token}");
    vec![
        // Most common on modern desktop Linux.
        LaunchAttempt {
            label: "gnome-terminal",
            program: PathBuf::from("gnome-terminal"),
            args: vec![
                "--".to_string(),
                "sh".to_string(),
                "-c".to_string(),
                cmd.clone(),
            ],
        },
        LaunchAttempt {
            label: "konsole",
            program: PathBuf::from("konsole"),
            args: vec![
                "-e".to_string(),
                "sh".to_string(),
                "-c".to_string(),
                cmd.clone(),
            ],
        },
        LaunchAttempt {
            label: "alacritty",
            program: PathBuf::from("alacritty"),
            args: vec![
                "-e".to_string(),
                "sh".to_string(),
                "-c".to_string(),
                cmd.clone(),
            ],
        },
        LaunchAttempt {
            label: "xterm",
            program: PathBuf::from("xterm"),
            args: vec!["-e".to_string(), "sh".to_string(), "-c".to_string(), cmd],
        },
    ]
}

/// Print the manual attach command to stderr/stdout so the user can run it
/// themselves if no auto-launcher worked.
pub fn print_manual_attach_hint(addr: &str, token: &str) {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "cli-bridge".to_string());
    warn!("No terminal launcher succeeded. To attach manually, open a terminal and run:");
    println!();
    println!("    {exe} --attach {addr} --attach-token {token}");
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_attempts_nonempty() {
        // Smoke-test: every supported platform produces at least one attempt
        // with a non-empty label and program.
        let attempts = launch_attempts("/bin/cli-bridge", "127.0.0.1:1234", "tok");
        assert!(!attempts.is_empty());
        for a in &attempts {
            assert!(!a.label.is_empty());
            assert!(!a.program.as_os_str().is_empty());
            // Each invocation must somewhere reference our binary.
            let joined = a.args.join(" ");
            assert!(
                joined.contains("cli-bridge") || a.program.to_string_lossy().contains("cli-bridge"),
                "neither program nor args reference cli-bridge: program={:?} args={:?}",
                a.program,
                a.args
            );
        }
    }
}
