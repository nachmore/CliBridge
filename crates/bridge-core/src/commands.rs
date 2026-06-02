use serde::{Deserialize, Serialize};

use crate::types::TerminalSize;

/// Special commands that can be sent from the messaging platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpecialCommand {
    /// Send a key press, optionally with modifiers, repeated `count` times.
    /// Covers all of `--ctrl+<key>`, `--alt+<key>`, `--shift+<key>` (and
    /// combinations) plus bare named keys like `--up`, `--pageup`, `--f5`,
    /// `--tab`, `--esc`. A trailing number repeats the key: `--up5` sends five
    /// Up presses, `--ctrl+left3` sends Ctrl+Left three times. `count` is at
    /// least 1.
    Key {
        key: Key,
        mods: Modifiers,
        count: u32,
    },
    /// Kill the shell process
    Kill,
    /// Restart the shell process. The bool `force` distinguishes a plain
    /// `--new` (asks for confirmation while a shell is alive) from
    /// `--new force` (proceeds immediately). When the shell has already
    /// exited, the bridge treats both as immediate. Aliased to `--new`
    /// in input parsing.
    Restart { force: bool },
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
    /// Send a bare carriage return ("press Enter"). Useful when an app is
    /// waiting on a confirmation prompt and you don't want to type any text.
    Enter,
    /// Show help for available commands. `topic = Some("config")` means
    /// "show the list of configurable settings" (delegated to the
    /// settings registry on the cli-bridge side); `None` is the
    /// general top-level help.
    Help { topic: Option<String> },
    /// Rename the session — affects the labels in lifecycle banners
    /// (started / restarted / exited / killed). Argument is the new name.
    Name(String),
    /// Read or write a runtime-mutable setting. Variants:
    ///   - `Config { key: None, .. }`        — list all settings
    ///   - `Config { key: Some(k), value: None }`        — show one
    ///   - `Config { key: Some(k), value: Some(v) }`    — set one
    Config {
        key: Option<String>,
        value: Option<String>,
    },
}

/// A single keyboard key, independent of modifiers. This is the unit that
/// [`encode_key`] turns into the bytes a terminal would emit when the key is
/// pressed. `Char` covers ordinary printable keys; the rest are the named
/// keys that have dedicated escape sequences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Key {
    Char(char),
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    Tab,
    Escape,
    Enter,
    Space,
    Backspace,
    /// Function key F1–F12.
    F(u8),
}

/// Keyboard modifiers that can accompany a [`Key`]. `none()` means an
/// unmodified press.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Modifiers {
    pub fn none() -> Self {
        Self::default()
    }

    fn is_none(self) -> bool {
        !self.ctrl && !self.alt && !self.shift
    }

    /// The xterm modifier parameter used in CSI sequences like `\x1b[1;5A`
    /// (Ctrl+Up). Encoded as 1 + shift(1) + alt(2) + ctrl(4).
    fn csi_param(self) -> u8 {
        1 + (self.shift as u8) + ((self.alt as u8) << 1) + ((self.ctrl as u8) << 2)
    }
}

/// Encode a key press (with optional modifiers) into the byte sequence a
/// terminal emits for it. Returns `None` when the combination has no
/// meaningful encoding (e.g. Ctrl with a key that has no control code).
///
/// Named keys use standard xterm sequences; with modifiers they take the
/// parametrized CSI form (`\x1b[1;<m>A` for cursor/F1–F4-style keys, and
/// `\x1b[<n>;<m>~` for tilde keys like PageUp). `Char` keys map Ctrl to the
/// usual C0 control byte and Alt to an ESC prefix.
pub fn encode_key(key: &Key, mods: Modifiers) -> Option<Vec<u8>> {
    match key {
        Key::Char(c) => encode_char(*c, mods),
        _ => encode_named(key, mods),
    }
}

fn encode_char(c: char, mods: Modifiers) -> Option<Vec<u8>> {
    // Shift on a letter just selects the uppercase form; for other glyphs the
    // typed character already reflects shift, so this is a no-op there.
    let c = if mods.shift { shift_char(c) } else { c };

    let mut bytes = if mods.ctrl {
        vec![ctrl_byte(c)?]
    } else {
        let mut buf = [0u8; 4];
        c.encode_utf8(&mut buf).as_bytes().to_vec()
    };

    // Alt/Meta is the ESC prefix on the resulting byte(s).
    if mods.alt {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

/// Map a character to its C0 control byte (Ctrl+key). Covers `@A–Z[\]^_`
/// (0x40–0x5f → 0x00–0x1f), plus Space (NUL) and `?` (DEL). Returns `None`
/// for characters that have no control encoding.
fn ctrl_byte(c: char) -> Option<u8> {
    let up = c.to_ascii_uppercase();
    match up {
        '@'..='_' => Some((up as u8) & 0x1f),
        ' ' => Some(0x00),
        '?' => Some(0x7f),
        _ => None,
    }
}

fn shift_char(c: char) -> char {
    if c.is_ascii_alphabetic() {
        c.to_ascii_uppercase()
    } else {
        c
    }
}

fn encode_named(key: &Key, mods: Modifiers) -> Option<Vec<u8>> {
    // Single-byte keys whose modifier behavior is special-cased.
    match key {
        Key::Tab => {
            // Shift+Tab is back-tab (CSI Z); otherwise HT, with optional ESC
            // prefix for Alt.
            if mods.shift {
                return Some(b"\x1b[Z".to_vec());
            }
            return Some(alt_prefixed(vec![0x09], mods));
        }
        Key::Enter => return Some(alt_prefixed(vec![0x0d], mods)),
        Key::Escape => return Some(alt_prefixed(vec![0x1b], mods)),
        Key::Backspace => {
            let b = if mods.ctrl { 0x08 } else { 0x7f };
            return Some(alt_prefixed(vec![b], mods));
        }
        Key::Space => return encode_char(' ', mods),
        _ => {}
    }

    // CSI keys: either a final-letter form (cursor + F1–F4) or a numeric
    // tilde form (PageUp/Down, Insert, Delete, F5+).
    let csi = match key {
        Key::Up => Csi::Letter(b'A'),
        Key::Down => Csi::Letter(b'B'),
        Key::Right => Csi::Letter(b'C'),
        Key::Left => Csi::Letter(b'D'),
        Key::Home => Csi::Letter(b'H'),
        Key::End => Csi::Letter(b'F'),
        Key::PageUp => Csi::Tilde(5),
        Key::PageDown => Csi::Tilde(6),
        Key::Insert => Csi::Tilde(2),
        Key::Delete => Csi::Tilde(3),
        Key::F(n) => match n {
            1 => Csi::Ss3(b'P'),
            2 => Csi::Ss3(b'Q'),
            3 => Csi::Ss3(b'R'),
            4 => Csi::Ss3(b'S'),
            5 => Csi::Tilde(15),
            6 => Csi::Tilde(17),
            7 => Csi::Tilde(18),
            8 => Csi::Tilde(19),
            9 => Csi::Tilde(20),
            10 => Csi::Tilde(21),
            11 => Csi::Tilde(23),
            12 => Csi::Tilde(24),
            _ => return None,
        },
        // Char and the single-byte keys above are handled elsewhere.
        Key::Char(_)
        | Key::Tab
        | Key::Enter
        | Key::Escape
        | Key::Backspace
        | Key::Space => return None,
    };
    Some(csi.encode(mods))
}

/// Prepend the ESC byte when Alt is held; otherwise return bytes unchanged.
fn alt_prefixed(mut bytes: Vec<u8>, mods: Modifiers) -> Vec<u8> {
    if mods.alt {
        bytes.insert(0, 0x1b);
    }
    bytes
}

/// The CSI shape of a named key, used to build both the plain and
/// modifier-parametrized escape sequences.
enum Csi {
    /// `\x1b[<final>` plain, `\x1b[1;<m><final>` with modifiers.
    Letter(u8),
    /// SS3 form `\x1bO<final>` plain (F1–F4), `\x1b[1;<m><final>` with mods.
    Ss3(u8),
    /// `\x1b[<n>~` plain, `\x1b[<n>;<m>~` with modifiers.
    Tilde(u8),
}

impl Csi {
    fn encode(&self, mods: Modifiers) -> Vec<u8> {
        let m = mods.csi_param();
        match self {
            Csi::Letter(fin) | Csi::Ss3(fin) if mods.is_none() => match self {
                Csi::Ss3(_) => vec![0x1b, b'O', *fin],
                _ => vec![0x1b, b'[', *fin],
            },
            Csi::Letter(fin) | Csi::Ss3(fin) => {
                format!("\x1b[1;{m}{}", *fin as char).into_bytes()
            }
            Csi::Tilde(n) if mods.is_none() => format!("\x1b[{n}~").into_bytes(),
            Csi::Tilde(n) => format!("\x1b[{n};{m}~").into_bytes(),
        }
    }
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
        // Legacy short aliases for the most common control keys. The general
        // --ctrl+<key> form below also handles these, but cc/cd/cz/cl are
        // muscle-memory shortcuts worth keeping.
        "cc" => ParsedInput::Command(key_cmd(Key::Char('c'), ctrl_mods(), 1)),
        "cd" => ParsedInput::Command(key_cmd(Key::Char('d'), ctrl_mods(), 1)),
        "cz" => ParsedInput::Command(key_cmd(Key::Char('z'), ctrl_mods(), 1)),
        "cl" => ParsedInput::Command(key_cmd(Key::Char('l'), ctrl_mods(), 1)),
        "kill" => ParsedInput::Command(SpecialCommand::Kill),
        "restart" | "new" => {
            let force = matches!(arg, Some(a) if a.eq_ignore_ascii_case("force"));
            ParsedInput::Command(SpecialCommand::Restart { force })
        }
        "clear" => ParsedInput::Command(SpecialCommand::Clear),
        "help" => ParsedInput::Command(SpecialCommand::Help {
            topic: arg.map(|a| a.to_string()),
        }),
        "config" => {
            // Three shapes: bare `--config`, `--config <key>`, `--config <key> <value...>`.
            // We use the *un-trimmed* arg here so a value like "  hello  "
            // round-trips through `name`'s own trim.
            match arg {
                None => ParsedInput::Command(SpecialCommand::Config {
                    key: None,
                    value: None,
                }),
                Some(rest) => {
                    let mut parts = rest.splitn(2, char::is_whitespace);
                    let key = parts.next().map(|s| s.to_string());
                    let value = parts.next().map(|s| s.to_string());
                    ParsedInput::Command(SpecialCommand::Config { key, value })
                }
            }
        }
        "enter" | "return" | "cr" => ParsedInput::Command(SpecialCommand::Enter),
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
        "name" => {
            // Multi-word names are fine: "--name my session" → "my session".
            if let Some(arg) = arg
                && !arg.is_empty()
            {
                ParsedInput::Command(SpecialCommand::Name(arg.to_string()))
            } else {
                ParsedInput::Text(input.to_string())
            }
        }
        // General key syntax: bare named keys (--up, --pageup, --f5, --tab,
        // --esc), modifier combos (--ctrl+c, --alt+shift+left, --ctrl+k), and
        // repeat counts (--up5, --ctrl+left3). Tried last so explicit commands
        // above always win.
        other => match parse_key_combo(other) {
            Some((key, mods, count)) => ParsedInput::Command(key_cmd(key, mods, count)),
            None => ParsedInput::Text(input.to_string()),
        },
    }
}

/// Upper bound on a key-repeat count, so a typo like `--up9999999` can't make
/// us synthesize a huge byte buffer.
const MAX_KEY_REPEAT: u32 = 1000;

fn key_cmd(key: Key, mods: Modifiers, count: u32) -> SpecialCommand {
    SpecialCommand::Key { key, mods, count }
}

fn ctrl_mods() -> Modifiers {
    Modifiers {
        ctrl: true,
        ..Modifiers::none()
    }
}

/// Parse a key spec like `ctrl+c`, `alt+shift+left`, `pageup`, `f5`, or
/// `up5` into a [`Key`], its [`Modifiers`], and a repeat count. The spec is
/// `+`-separated: every segment but the last is a modifier, and the last is
/// the key (with an optional trailing repeat count). Returns `None` if any
/// segment is unrecognized, so the caller can fall back to plain text.
///
/// Input is already lowercased by the caller.
fn parse_key_combo(spec: &str) -> Option<(Key, Modifiers, u32)> {
    // The key is the segment after the last `+` separator — but `+` is also a
    // valid key, so a spec ending in `+` (e.g. `ctrl++`) means the key is
    // literally `+` and everything before the final `+` is the modifier list.
    let (mod_part, key_str) = if let Some(stripped) = spec.strip_suffix('+')
        && !stripped.is_empty()
    {
        (stripped.trim_end_matches('+'), "+")
    } else {
        match spec.rsplit_once('+') {
            Some((mods, key)) => (mods, key),
            None => ("", spec),
        }
    };

    if key_str.is_empty() {
        return None;
    }

    let mut mods = Modifiers::none();
    if !mod_part.is_empty() {
        for seg in mod_part.split('+') {
            match seg {
                "ctrl" | "control" | "c" => mods.ctrl = true,
                "alt" | "meta" | "opt" | "option" | "a" | "m" => mods.alt = true,
                "shift" | "s" => mods.shift = true,
                _ => return None,
            }
        }
    }

    let (key, count) = parse_key_with_count(key_str)?;
    Some((key, mods, count))
}

/// Resolve a key token that may carry a trailing repeat count, e.g. `up5`.
/// The whole token is tried as a key name first, so `f5`/`f12` stay function
/// keys rather than being read as `f` repeated. Only if that fails do we peel
/// a trailing digit run as the count (`up5` → Up ×5, `ctrl+left3` → Left ×3).
/// A count of 0 is rejected; anything above [`MAX_KEY_REPEAT`] is clamped.
fn parse_key_with_count(token: &str) -> Option<(Key, u32)> {
    if let Some(key) = parse_key_name(token) {
        return Some((key, 1));
    }

    // Trailing ASCII digits are the count; digits are single-byte so the
    // char count is a valid byte split point.
    let trailing_digits = token.chars().rev().take_while(|c| c.is_ascii_digit()).count();
    if trailing_digits == 0 {
        return None;
    }
    let split = token.len() - trailing_digits;
    let (base, num) = token.split_at(split);
    if base.is_empty() {
        return None;
    }
    let count: u32 = num.parse().ok()?;
    if count == 0 {
        return None;
    }
    let key = parse_key_name(base)?;
    Some((key, count.min(MAX_KEY_REPEAT)))
}

/// Map a key name to a [`Key`]. Single characters become `Key::Char`; longer
/// tokens are matched against the named-key table. Input is lowercased.
fn parse_key_name(name: &str) -> Option<Key> {
    let mut chars = name.chars();
    let first = chars.next()?;
    if chars.next().is_none() {
        // Single character: a literal key press.
        return Some(Key::Char(first));
    }

    // Function keys: f1..=f12.
    if let Some(num) = name.strip_prefix('f')
        && let Ok(n) = num.parse::<u8>()
        && (1..=12).contains(&n)
    {
        return Some(Key::F(n));
    }

    let key = match name {
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "pgup" => Key::PageUp,
        "pagedown" | "pgdn" | "pgdown" => Key::PageDown,
        "insert" | "ins" => Key::Insert,
        "delete" | "del" => Key::Delete,
        "tab" => Key::Tab,
        "esc" | "escape" => Key::Escape,
        "enter" | "return" | "cr" => Key::Enter,
        "space" | "spc" => Key::Space,
        "backspace" | "bksp" | "bs" => Key::Backspace,
        _ => return None,
    };
    Some(key)
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
        SpecialCommand::Key { key, mods, count } => {
            let one = encode_key(key, *mods)?;
            Some(one.repeat((*count).max(1) as usize))
        }
        SpecialCommand::Enter => Some(vec![b'\r']),
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
        | SpecialCommand::Restart { .. }
        | SpecialCommand::Resize(_)
        | SpecialCommand::Clear
        | SpecialCommand::Help { .. }
        | SpecialCommand::Name(_)
        | SpecialCommand::Config { .. } => None,
    }
}

/// Generate help text listing all available commands.
pub fn help_text() -> String {
    r#"*CliBridge Commands:*
• `--ctrl+<key>` — Send a key with Ctrl (e.g. `--ctrl+c` interrupt, `--ctrl+d` EOF, `--ctrl+z` suspend, `--ctrl+l` clear, `--ctrl+\` SIGQUIT). Shortcuts: `--cc` `--cd` `--cz` `--cl`.
• `--alt+<key>` / `--shift+<key>` — Send a key with Alt or Shift. Combine them: `--ctrl+alt+del`, `--alt+shift+left`.
• `--<key>` — Send a named key on its own: `--up` `--down` `--left` `--right`, `--home` `--end`, `--pageup` `--pagedown`, `--insert` `--delete`, `--tab`, `--esc`, `--space`, `--backspace`, `--f1`…`--f12`.
• `--<key><n>` — Repeat a key `n` times: `--up5` sends five Up presses, `--ctrl+left3` sends Ctrl+Left three times.
• `--kill` — Kill the shell process
• `--restart` / `--new` — (Re)spawn the shell. While a shell is alive, asks for confirmation; reply `--new force` to terminate the current one and start fresh. After the shell has exited, plain `--new` works.
• `--resize 120x40` — Resize terminal (cols x rows)
• `--clear` — Re-anchor: end the current edited message, start a fresh one on next output
• `--enter` — Send a bare Enter (CR), no text. Aliases: `--return`, `--cr`
• `--tmux <key>` — Send tmux prefix + key
• `--raw <hex>` — Send raw bytes (hex-encoded)
• `--slash <name>` — Send a literal `/name` to the shell (e.g. `--slash init` for Claude Code)
• `--name <text>` — Rename the session (shows up in start/exit banners). Shortcut for `--config name <text>`.
• `--config` — List runtime-mutable settings.
• `--config <key>` — Show one setting's current value.
• `--config <key> <value>` — Set a setting (e.g. `--config show_cursor off`).
• `--help` — Show this help. `--help config` lists every configurable setting with descriptions.

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
        let ctrl_c = SpecialCommand::Key {
            key: Key::Char('c'),
            mods: Modifiers {
                ctrl: true,
                ..Modifiers::none()
            },
            count: 1,
        };
        assert_eq!(parse_input("--ctrl+c"), ParsedInput::Command(ctrl_c.clone()));
        // Legacy short alias still works.
        assert_eq!(parse_input("--cc"), ParsedInput::Command(ctrl_c.clone()));
        // Case-insensitive on the modifier and key.
        assert_eq!(parse_input("--CTRL+C"), ParsedInput::Command(ctrl_c));
    }

    #[test]
    fn test_parse_modifier_combos() {
        // alt+shift+left → CSI param 1 + shift(1) + alt(2) = 4
        assert_eq!(
            parse_input("--alt+shift+left"),
            ParsedInput::Command(SpecialCommand::Key {
                key: Key::Left,
                mods: Modifiers {
                    alt: true,
                    shift: true,
                    ..Modifiers::none()
                },
                count: 1,
            })
        );
        // Bare named key, no modifiers.
        assert_eq!(
            parse_input("--pageup"),
            ParsedInput::Command(SpecialCommand::Key {
                key: Key::PageUp,
                mods: Modifiers::none(),
                count: 1,
            })
        );
        // Function key.
        assert_eq!(
            parse_input("--f5"),
            ParsedInput::Command(SpecialCommand::Key {
                key: Key::F(5),
                mods: Modifiers::none(),
                count: 1,
            })
        );
        // Arrow keys still parse.
        assert_eq!(
            parse_input("--up"),
            ParsedInput::Command(SpecialCommand::Key {
                key: Key::Up,
                mods: Modifiers::none(),
                count: 1,
            })
        );
        // tab/esc as named keys.
        assert_eq!(
            parse_input("--tab"),
            ParsedInput::Command(SpecialCommand::Key {
                key: Key::Tab,
                mods: Modifiers::none(),
                count: 1,
            })
        );
        // ctrl++ → the literal '+' key with ctrl.
        assert_eq!(
            parse_input("--ctrl++"),
            ParsedInput::Command(SpecialCommand::Key {
                key: Key::Char('+'),
                mods: Modifiers {
                    ctrl: true,
                    ..Modifiers::none()
                },
                count: 1,
            })
        );
    }

    #[test]
    fn test_parse_unknown_key_combo_is_text() {
        // Unrecognized modifier → plain text, not a command.
        assert_eq!(
            parse_input("--hyper+x"),
            ParsedInput::Text("--hyper+x".to_string())
        );
        // Unrecognized multi-char key name → plain text.
        assert_eq!(
            parse_input("--frobnicate"),
            ParsedInput::Text("--frobnicate".to_string())
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
    fn test_parse_config_three_shapes() {
        // Bare --config: list mode.
        assert_eq!(
            parse_input("--config"),
            ParsedInput::Command(SpecialCommand::Config {
                key: None,
                value: None
            })
        );
        // --config <key>: read one.
        assert_eq!(
            parse_input("--config show_cursor"),
            ParsedInput::Command(SpecialCommand::Config {
                key: Some("show_cursor".to_string()),
                value: None
            })
        );
        // --config <key> <value>: set one. Value can contain spaces.
        assert_eq!(
            parse_input("--config name my session"),
            ParsedInput::Command(SpecialCommand::Config {
                key: Some("name".to_string()),
                value: Some("my session".to_string())
            })
        );
    }

    #[test]
    fn test_parse_help_with_topic() {
        assert_eq!(
            parse_input("--help"),
            ParsedInput::Command(SpecialCommand::Help { topic: None })
        );
        assert_eq!(
            parse_input("--help config"),
            ParsedInput::Command(SpecialCommand::Help {
                topic: Some("config".to_string())
            })
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
    fn test_parse_enter_aliases() {
        for alias in ["--enter", "--return", "--cr", "--ENTER"] {
            assert_eq!(
                parse_input(alias),
                ParsedInput::Command(SpecialCommand::Enter),
                "alias {alias} did not parse"
            );
        }
    }

    #[test]
    fn test_command_to_bytes_enter() {
        assert_eq!(command_to_bytes(&SpecialCommand::Enter), Some(vec![b'\r']));
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
        assert_eq!(
            command_to_bytes(&SpecialCommand::Key {
                key: Key::Char('c'),
                mods: Modifiers {
                    ctrl: true,
                    ..Modifiers::none()
                },
                count: 1,
            }),
            Some(vec![0x03])
        );
    }

    #[test]
    fn test_command_to_bytes_arrow() {
        assert_eq!(
            command_to_bytes(&SpecialCommand::Key {
                key: Key::Up,
                mods: Modifiers::none(),
                count: 1,
            }),
            Some(b"\x1b[A".to_vec())
        );
    }

    #[test]
    fn test_parse_repeat_counts() {
        // --up5 → Up repeated 5 times.
        assert_eq!(
            parse_input("--up5"),
            ParsedInput::Command(SpecialCommand::Key {
                key: Key::Up,
                mods: Modifiers::none(),
                count: 5,
            })
        );
        // Modifier + repeat: --ctrl+left3.
        assert_eq!(
            parse_input("--ctrl+left3"),
            ParsedInput::Command(SpecialCommand::Key {
                key: Key::Left,
                mods: Modifiers {
                    ctrl: true,
                    ..Modifiers::none()
                },
                count: 3,
            })
        );
        // f5 stays a function key (not "f" times 5).
        assert_eq!(
            parse_input("--f5"),
            ParsedInput::Command(SpecialCommand::Key {
                key: Key::F(5),
                mods: Modifiers::none(),
                count: 1,
            })
        );
        // count is clamped to MAX_KEY_REPEAT.
        assert_eq!(
            parse_input("--down99999"),
            ParsedInput::Command(SpecialCommand::Key {
                key: Key::Down,
                mods: Modifiers::none(),
                count: MAX_KEY_REPEAT,
            })
        );
    }

    #[test]
    fn test_command_to_bytes_repeated_arrow() {
        // --up3 emits the Up sequence three times back-to-back.
        assert_eq!(
            command_to_bytes(&SpecialCommand::Key {
                key: Key::Up,
                mods: Modifiers::none(),
                count: 3,
            }),
            Some(b"\x1b[A\x1b[A\x1b[A".to_vec())
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
    fn test_parse_name() {
        assert_eq!(
            parse_input("--name my-session"),
            ParsedInput::Command(SpecialCommand::Name("my-session".to_string()))
        );
        // Multi-word names: keep the whole tail.
        assert_eq!(
            parse_input("--name build server   us-east"),
            ParsedInput::Command(SpecialCommand::Name("build server   us-east".to_string()))
        );
        // Empty arg → falls through to plain text (avoids silently clearing
        // the name on a typo).
        assert_eq!(
            parse_input("--name"),
            ParsedInput::Text("--name".to_string())
        );
        assert_eq!(
            parse_input("--name "),
            ParsedInput::Text("--name ".to_string())
        );
    }

    #[test]
    fn test_command_to_bytes_name_returns_none() {
        // Name is a control command, no PTY bytes.
        assert_eq!(
            command_to_bytes(&SpecialCommand::Name("foo".to_string())),
            None
        );
    }

    #[test]
    fn test_parse_new_aliases_restart() {
        assert_eq!(
            parse_input("--new"),
            ParsedInput::Command(SpecialCommand::Restart { force: false })
        );
        assert_eq!(
            parse_input("--restart"),
            ParsedInput::Command(SpecialCommand::Restart { force: false })
        );
    }

    #[test]
    fn test_parse_new_force() {
        assert_eq!(
            parse_input("--new force"),
            ParsedInput::Command(SpecialCommand::Restart { force: true })
        );
        // Case-insensitive
        assert_eq!(
            parse_input("--new FORCE"),
            ParsedInput::Command(SpecialCommand::Restart { force: true })
        );
        // Anything else is treated as not-force
        assert_eq!(
            parse_input("--new please"),
            ParsedInput::Command(SpecialCommand::Restart { force: false })
        );
    }

    #[test]
    fn test_encode_key_plain_char() {
        assert_eq!(
            encode_key(&Key::Char('a'), Modifiers::none()),
            Some(b"a".to_vec())
        );
    }

    #[test]
    fn test_encode_key_ctrl_letters() {
        // Ctrl+C/D/Z/L/\ match the legacy hardcoded control bytes.
        assert_eq!(
            encode_key(&Key::Char('c'), Modifiers { ctrl: true, ..Modifiers::none() }),
            Some(vec![0x03])
        );
        assert_eq!(
            encode_key(&Key::Char('d'), Modifiers { ctrl: true, ..Modifiers::none() }),
            Some(vec![0x04])
        );
        assert_eq!(
            encode_key(&Key::Char('z'), Modifiers { ctrl: true, ..Modifiers::none() }),
            Some(vec![0x1a])
        );
        assert_eq!(
            encode_key(&Key::Char('l'), Modifiers { ctrl: true, ..Modifiers::none() }),
            Some(vec![0x0c])
        );
        assert_eq!(
            encode_key(&Key::Char('\\'), Modifiers { ctrl: true, ..Modifiers::none() }),
            Some(vec![0x1c])
        );
        // Ctrl is case-insensitive on letters.
        assert_eq!(
            encode_key(&Key::Char('C'), Modifiers { ctrl: true, ..Modifiers::none() }),
            Some(vec![0x03])
        );
    }

    #[test]
    fn test_encode_key_alt_char_is_esc_prefixed() {
        assert_eq!(
            encode_key(&Key::Char('a'), Modifiers { alt: true, ..Modifiers::none() }),
            Some(vec![0x1b, b'a'])
        );
    }

    #[test]
    fn test_encode_key_arrows_plain() {
        assert_eq!(encode_key(&Key::Up, Modifiers::none()), Some(b"\x1b[A".to_vec()));
        assert_eq!(encode_key(&Key::Down, Modifiers::none()), Some(b"\x1b[B".to_vec()));
        assert_eq!(encode_key(&Key::Right, Modifiers::none()), Some(b"\x1b[C".to_vec()));
        assert_eq!(encode_key(&Key::Left, Modifiers::none()), Some(b"\x1b[D".to_vec()));
    }

    #[test]
    fn test_encode_key_arrows_with_modifiers() {
        // Ctrl+Up → CSI 1;5 A
        assert_eq!(
            encode_key(&Key::Up, Modifiers { ctrl: true, ..Modifiers::none() }),
            Some(b"\x1b[1;5A".to_vec())
        );
        // Shift+Right → CSI 1;2 C
        assert_eq!(
            encode_key(&Key::Right, Modifiers { shift: true, ..Modifiers::none() }),
            Some(b"\x1b[1;2C".to_vec())
        );
        // Ctrl+Alt+Left → 1 + alt(2) + ctrl(4) = 7
        assert_eq!(
            encode_key(&Key::Left, Modifiers { ctrl: true, alt: true, ..Modifiers::none() }),
            Some(b"\x1b[1;7D".to_vec())
        );
    }

    #[test]
    fn test_encode_key_tilde_keys() {
        assert_eq!(encode_key(&Key::PageUp, Modifiers::none()), Some(b"\x1b[5~".to_vec()));
        assert_eq!(encode_key(&Key::PageDown, Modifiers::none()), Some(b"\x1b[6~".to_vec()));
        assert_eq!(encode_key(&Key::Delete, Modifiers::none()), Some(b"\x1b[3~".to_vec()));
        // Ctrl+PageUp → CSI 5;5 ~
        assert_eq!(
            encode_key(&Key::PageUp, Modifiers { ctrl: true, ..Modifiers::none() }),
            Some(b"\x1b[5;5~".to_vec())
        );
    }

    #[test]
    fn test_encode_key_function_keys() {
        assert_eq!(encode_key(&Key::F(1), Modifiers::none()), Some(b"\x1bOP".to_vec()));
        assert_eq!(encode_key(&Key::F(5), Modifiers::none()), Some(b"\x1b[15~".to_vec()));
        assert_eq!(encode_key(&Key::F(12), Modifiers::none()), Some(b"\x1b[24~".to_vec()));
        assert_eq!(encode_key(&Key::F(13), Modifiers::none()), None);
    }

    #[test]
    fn test_encode_key_shift_tab_is_backtab() {
        assert_eq!(
            encode_key(&Key::Tab, Modifiers { shift: true, ..Modifiers::none() }),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(encode_key(&Key::Tab, Modifiers::none()), Some(vec![0x09]));
    }

    #[test]
    fn test_encode_key_ctrl_on_plain_digit_is_none() {
        // Ctrl+1 has no control code.
        assert_eq!(
            encode_key(&Key::Char('1'), Modifiers { ctrl: true, ..Modifiers::none() }),
            None
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
