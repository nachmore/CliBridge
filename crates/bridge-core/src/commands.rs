use serde::{Deserialize, Serialize};

use crate::types::TerminalSize;

/// Special commands that can be sent from the messaging platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpecialCommand {
    /// Send Ctrl+C (SIGINT)
    CtrlC,
    /// Send Ctrl+D (EOF)
    CtrlD,
    /// Send Ctrl+Z (SIGTSTP)
    CtrlZ,
    /// Send Ctrl+L (clear screen)
    CtrlL,
    /// Send Ctrl+\ (SIGQUIT)
    CtrlBackslash,
    /// Kill the shell process
    Kill,
    /// Restart the shell process. Same effect whether the shell is alive
    /// (kill + respawn) or already exited (just spawn). Aliased to `--new`
    /// in input parsing, since "new" reads better when the previous shell
    /// has already died.
    Restart,
    /// Resize the terminal
    Resize(TerminalSize),
    /// Clear the message history in the channel
    Clear,
    /// Send a raw escape sequence (hex-encoded)
    Raw(String),
    /// Send a literal slash command to the shell, e.g. `/init` for Claude Code.
    /// The argument is the text *after* the slash; we add the `/` prefix and a
    /// CR so the shell submits the line. Slack would otherwise eat anything
    /// starting with `/` as a native slash command before it ever reaches us.
    Slash(String),
    /// Send tmux prefix (Ctrl+B by default) followed by a key
    Tmux(String),
    /// Send an arrow key
    Arrow(ArrowDirection),
    /// Send Tab
    Tab,
    /// Send Escape
    Escape,
    /// Show help for available commands
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArrowDirection {
    Up,
    Down,
    Left,
    Right,
}

/// Result of parsing user input from the messaging platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedInput {
    /// A special command was recognized
    Command(SpecialCommand),
    /// Plain text to send as terminal input (with newline appended)
    Text(String),
}

const COMMAND_PREFIX: &str = "--";

/// Parse a message from the messaging platform into either a command or text input.
pub fn parse_input(input: &str) -> ParsedInput {
    let trimmed = input.trim();

    let Some(rest) = trimmed.strip_prefix(COMMAND_PREFIX) else {
        return ParsedInput::Text(input.to_string());
    };

    let parts: Vec<&str> = rest.splitn(2, ' ').collect();
    let cmd = parts[0].to_lowercase();
    let arg = parts.get(1).map(|s| s.trim());

    match cmd.as_str() {
        "ctrl+c" | "ctrlc" | "cc" => ParsedInput::Command(SpecialCommand::CtrlC),
        "ctrl+d" | "ctrld" | "cd" => ParsedInput::Command(SpecialCommand::CtrlD),
        "ctrl+z" | "ctrlz" | "cz" => ParsedInput::Command(SpecialCommand::CtrlZ),
        "ctrl+l" | "ctrll" | "cl" => ParsedInput::Command(SpecialCommand::CtrlL),
        "ctrl+\\" | "ctrlbs" => ParsedInput::Command(SpecialCommand::CtrlBackslash),
        "kill" => ParsedInput::Command(SpecialCommand::Kill),
        "restart" | "new" => ParsedInput::Command(SpecialCommand::Restart),
        "clear" => ParsedInput::Command(SpecialCommand::Clear),
        "help" => ParsedInput::Command(SpecialCommand::Help),
        "tab" => ParsedInput::Command(SpecialCommand::Tab),
        "esc" | "escape" => ParsedInput::Command(SpecialCommand::Escape),
        "up" => ParsedInput::Command(SpecialCommand::Arrow(ArrowDirection::Up)),
        "down" => ParsedInput::Command(SpecialCommand::Arrow(ArrowDirection::Down)),
        "left" => ParsedInput::Command(SpecialCommand::Arrow(ArrowDirection::Left)),
        "right" => ParsedInput::Command(SpecialCommand::Arrow(ArrowDirection::Right)),
        "resize" => {
            if let Some(arg) = arg
                && let Some(size) = parse_size(arg)
            {
                return ParsedInput::Command(SpecialCommand::Resize(size));
            }
            ParsedInput::Text(input.to_string())
        }
        "raw" => {
            if let Some(arg) = arg {
                ParsedInput::Command(SpecialCommand::Raw(arg.to_string()))
            } else {
                ParsedInput::Text(input.to_string())
            }
        }
        "slash" => {
            // Accept --slash <name> with optional args. The leading "/" is
            // re-added by command_to_bytes, so users type --slash init, not
            // --slash /init (though we tolerate the latter).
            if let Some(arg) = arg {
                let stripped = arg.strip_prefix('/').unwrap_or(arg);
                ParsedInput::Command(SpecialCommand::Slash(stripped.to_string()))
            } else {
                ParsedInput::Text(input.to_string())
            }
        }
        "tmux" => {
            if let Some(arg) = arg {
                ParsedInput::Command(SpecialCommand::Tmux(arg.to_string()))
            } else {
                // Just send the tmux prefix
                ParsedInput::Command(SpecialCommand::Tmux(String::new()))
            }
        }
        _ => ParsedInput::Text(input.to_string()),
    }
}

fn parse_size(s: &str) -> Option<TerminalSize> {
    let parts: Vec<&str> = s.split('x').collect();
    if parts.len() == 2 {
        let cols = parts[0].trim().parse().ok()?;
        let rows = parts[1].trim().parse().ok()?;
        Some(TerminalSize { cols, rows })
    } else {
        None
    }
}

/// Convert a special command to the bytes that should be sent to the terminal.
pub fn command_to_bytes(cmd: &SpecialCommand) -> Option<Vec<u8>> {
    match cmd {
        SpecialCommand::CtrlC => Some(vec![0x03]),
        SpecialCommand::CtrlD => Some(vec![0x04]),
        SpecialCommand::CtrlZ => Some(vec![0x1A]),
        SpecialCommand::CtrlL => Some(vec![0x0C]),
        SpecialCommand::CtrlBackslash => Some(vec![0x1C]),
        SpecialCommand::Tab => Some(vec![0x09]),
        SpecialCommand::Escape => Some(vec![0x1B]),
        SpecialCommand::Arrow(dir) => {
            let seq = match dir {
                ArrowDirection::Up => b"\x1b[A".to_vec(),
                ArrowDirection::Down => b"\x1b[B".to_vec(),
                ArrowDirection::Right => b"\x1b[C".to_vec(),
                ArrowDirection::Left => b"\x1b[D".to_vec(),
            };
            Some(seq)
        }
        SpecialCommand::Raw(hex) => hex::decode(hex).ok(),
        SpecialCommand::Slash(name) => {
            // Build "/<name>\r". CR submits the line in ConPTY (cmd / PowerShell
            // need CR; Unix shells accept it too) — same convention as plain
            // text input from Slack.
            let mut bytes = Vec::with_capacity(name.len() + 2);
            bytes.push(b'/');
            bytes.extend_from_slice(name.as_bytes());
            bytes.push(b'\r');
            Some(bytes)
        }
        SpecialCommand::Tmux(key) => {
            let mut bytes = vec![0x02]; // Ctrl+B (tmux prefix)
            if !key.is_empty() {
                bytes.extend_from_slice(key.as_bytes());
            }
            Some(bytes)
        }
        // These commands don't produce terminal bytes
        SpecialCommand::Kill
        | SpecialCommand::Restart
        | SpecialCommand::Resize(_)
        | SpecialCommand::Clear
        | SpecialCommand::Help => None,
    }
}

/// Generate help text listing all available commands.
pub fn help_text() -> String {
    r#"*CliBridge Commands:*
• `--ctrl+c` — Send interrupt (SIGINT)
• `--ctrl+d` — Send EOF
• `--ctrl+z` — Suspend (SIGTSTP)
• `--ctrl+l` — Clear screen
• `--ctrl+\` — Send SIGQUIT
• `--kill` — Kill the shell process
• `--restart` / `--new` — (Re)spawn the shell. `--new` is a friendly alias when the previous shell has exited.
• `--resize 120x40` — Resize terminal (cols x rows)
• `--clear` — Clear message history
• `--tab` — Send Tab key
• `--esc` — Send Escape key
• `--up` `--down` `--left` `--right` — Arrow keys
• `--tmux <key>` — Send tmux prefix + key
• `--raw <hex>` — Send raw bytes (hex-encoded)
• `--slash <name>` — Send a literal `/name` to the shell (e.g. `--slash init` for Claude Code)
• `--help` — Show this help

Any other text is sent directly as terminal input."#
        .to_string()
}

/// Hex decode helper (minimal, avoids adding hex crate dependency)
mod hex {
    pub fn decode(s: &str) -> Result<Vec<u8>, ()> {
        let s = s.trim();
        if !s.len().is_multiple_of(2) {
            return Err(());
        }
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| ()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ctrl_c() {
        assert_eq!(
            parse_input("--ctrl+c"),
            ParsedInput::Command(SpecialCommand::CtrlC)
        );
        assert_eq!(
            parse_input("--cc"),
            ParsedInput::Command(SpecialCommand::CtrlC)
        );
    }

    #[test]
    fn test_parse_plain_text() {
        assert_eq!(
            parse_input("ls -la"),
            ParsedInput::Text("ls -la".to_string())
        );
    }

    #[test]
    fn test_parse_slash_is_text() {
        // Slash messages are Slack slash commands and shouldn't reach us, but
        // if they do we treat them as plain text.
        assert_eq!(parse_input("/help"), ParsedInput::Text("/help".to_string()));
    }

    #[test]
    fn test_parse_resize() {
        assert_eq!(
            parse_input("--resize 120x40"),
            ParsedInput::Command(SpecialCommand::Resize(TerminalSize {
                cols: 120,
                rows: 40
            }))
        );
    }

    #[test]
    fn test_parse_unknown_command_passes_through() {
        assert_eq!(
            parse_input("--unknown"),
            ParsedInput::Text("--unknown".to_string())
        );
    }

    #[test]
    fn test_parse_tmux() {
        assert_eq!(
            parse_input("--tmux d"),
            ParsedInput::Command(SpecialCommand::Tmux("d".to_string()))
        );
    }

    #[test]
    fn test_command_to_bytes_ctrl_c() {
        assert_eq!(command_to_bytes(&SpecialCommand::CtrlC), Some(vec![0x03]));
    }

    #[test]
    fn test_command_to_bytes_arrow() {
        assert_eq!(
            command_to_bytes(&SpecialCommand::Arrow(ArrowDirection::Up)),
            Some(b"\x1b[A".to_vec())
        );
    }

    #[test]
    fn test_command_to_bytes_kill_returns_none() {
        assert_eq!(command_to_bytes(&SpecialCommand::Kill), None);
    }

    #[test]
    fn test_hex_decode() {
        assert_eq!(
            command_to_bytes(&SpecialCommand::Raw("1b5b41".to_string())),
            Some(vec![0x1b, 0x5b, 0x41])
        );
    }

    #[test]
    fn test_parse_slash() {
        assert_eq!(
            parse_input("--slash init"),
            ParsedInput::Command(SpecialCommand::Slash("init".to_string()))
        );
        // Tolerate users typing the leading '/' anyway.
        assert_eq!(
            parse_input("--slash /model"),
            ParsedInput::Command(SpecialCommand::Slash("model".to_string()))
        );
        // Argument with spaces (e.g. /memory add ...).
        assert_eq!(
            parse_input("--slash memory add foo bar"),
            ParsedInput::Command(SpecialCommand::Slash("memory add foo bar".to_string()))
        );
        // No argument falls through to plain text.
        assert_eq!(
            parse_input("--slash"),
            ParsedInput::Text("--slash".to_string())
        );
    }

    #[test]
    fn test_parse_new_aliases_restart() {
        assert_eq!(
            parse_input("--new"),
            ParsedInput::Command(SpecialCommand::Restart)
        );
        assert_eq!(
            parse_input("--restart"),
            ParsedInput::Command(SpecialCommand::Restart)
        );
    }

    #[test]
    fn test_command_to_bytes_slash() {
        assert_eq!(
            command_to_bytes(&SpecialCommand::Slash("init".to_string())),
            Some(b"/init\r".to_vec())
        );
        assert_eq!(
            command_to_bytes(&SpecialCommand::Slash("memory add foo".to_string())),
            Some(b"/memory add foo\r".to_vec())
        );
    }
}
