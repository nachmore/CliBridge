//! Runtime-mutable settings driven from Slack via `--config`.
//!
//! Three operations: list (no args), read one (just key), write one (key
//! + value). The registry below is the single source of truth for which
//!   keys exist, their types, and a one-line description; all three
//!   operations and the `--help config` text iterate it.
//!
//! Why a flat enum-keyed dispatch instead of a closure-based registry:
//! the actual targets (renderer, anchor_refresh, name) live in
//! different parts of the session loop and have different mutability
//! requirements (some need `&mut TuiRenderer`, the name needs an
//! `Arc<Mutex<String>>`). A closure registry would force everything
//! into a single trait object and be more code, not less. The match
//! statements stay readable and the registry slice still drives `list`
//! and `--help config` from one place.
//!
//! Key naming: snake_case, matching the TOML/CLI spelling so a user
//! can copy `replace_block_chars = true` to/from their config file.

use std::sync::{Arc, Mutex};

use bridge_slack::TuiRenderer;

/// A single setting's metadata. Used to drive `--config` (no args) and
/// `--help config`. Read/write logic lives in `read_setting` and
/// `apply_setting` and matches against `name`.
pub struct SettingMeta {
    pub name: &'static str,
    pub kind: &'static str,
    pub description: &'static str,
}

/// All runtime-mutable settings. Adding a new setting means: append to
/// this slice, add a branch in `read_setting`, add a branch in
/// `apply_setting`, and (if the value lives outside the renderer) thread
/// the field through `RuntimeSettings`.
pub const SETTINGS: &[SettingMeta] = &[
    SettingMeta {
        name: "replace_block_chars",
        kind: "bool",
        description: "Replace U+2580–U+259F with ASCII so Slack's font fallback can't widen them.",
    },
    SettingMeta {
        name: "show_cursor",
        kind: "bool",
        description: "Render the terminal cursor as █ in the live frame.",
    },
    SettingMeta {
        name: "anchor_refresh",
        kind: "u32",
        description: "Re-anchor the live message every N inbound Slack messages. 0 disables.",
    },
    SettingMeta {
        name: "scroll_buffer",
        kind: "usize",
        description: "Lines of scroll buffer retained in memory before eviction. 0 disables.",
    },
    SettingMeta {
        name: "name",
        kind: "string",
        description: "Display name for this session, shown in lifecycle banners.",
    },
];

/// State carried by the session loop that the config handler can mutate.
/// Renderer is held mutably by the loop too, so it's passed alongside.
pub struct RuntimeSettings {
    pub anchor_refresh: u32,
    pub name: Arc<Mutex<String>>,
}

/// One-line description of a setting (or `None` if the name is unknown).
pub fn lookup(name: &str) -> Option<&'static SettingMeta> {
    SETTINGS.iter().find(|s| s.name == name)
}

/// Read a setting's current value as a display string. Returns `None`
/// for unknown keys.
pub fn read_setting(
    name: &str,
    renderer: &TuiRenderer,
    settings: &RuntimeSettings,
) -> Option<String> {
    match name {
        "replace_block_chars" => Some(renderer.replace_block_chars().to_string()),
        "show_cursor" => Some(renderer.show_cursor().to_string()),
        "anchor_refresh" => Some(settings.anchor_refresh.to_string()),
        "scroll_buffer" => Some(renderer.scroll_buffer_max().to_string()),
        "name" => Some(read_name(&settings.name)),
        _ => None,
    }
}

/// Apply a setting. Returns a one-line confirmation message on success
/// or an error message on bad input. Unknown keys are an error.
pub fn apply_setting(
    name: &str,
    value: &str,
    renderer: &mut TuiRenderer,
    settings: &mut RuntimeSettings,
) -> Result<String, String> {
    match name {
        "replace_block_chars" => {
            let v = parse_bool(value)?;
            renderer.set_replace_block_chars(v);
            Ok(format!("replace_block_chars = {v}"))
        }
        "show_cursor" => {
            let v = parse_bool(value)?;
            renderer.set_show_cursor(v);
            Ok(format!("show_cursor = {v}"))
        }
        "anchor_refresh" => {
            let v: u32 = value
                .parse()
                .map_err(|_| format!("anchor_refresh expects a non-negative integer, got {value:?}"))?;
            settings.anchor_refresh = v;
            Ok(format!("anchor_refresh = {v}"))
        }
        "scroll_buffer" => {
            let v: usize = value
                .parse()
                .map_err(|_| format!("scroll_buffer expects a non-negative integer, got {value:?}"))?;
            renderer.set_scroll_buffer_max(v);
            Ok(format!("scroll_buffer = {v}"))
        }
        "name" => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return Err("name must be non-empty".to_string());
            }
            if let Ok(mut g) = settings.name.lock() {
                *g = trimmed.to_string();
            }
            Ok(format!("name = *{trimmed}*"))
        }
        _ => Err(format!(
            "unknown setting `{name}`. Try `--help config` to see the list."
        )),
    }
}

/// Render the current value of every setting as a Slack-friendly mrkdwn
/// list. Used by `--config` with no args.
pub fn list_settings(renderer: &TuiRenderer, settings: &RuntimeSettings) -> String {
    let mut out = String::from("*Current settings:*\n");
    for s in SETTINGS {
        let val = read_setting(s.name, renderer, settings).unwrap_or_else(|| "?".to_string());
        out.push_str(&format!("• `{}` = `{}` _({})_\n", s.name, val, s.kind));
    }
    out.push_str(
        "\nSet with `--config <key> <value>`, read with `--config <key>`, list with `--config`. \
         `--help config` shows descriptions.",
    );
    out
}

/// Render the help text for `--help config`: each setting's name, type,
/// and description.
pub fn help_text() -> String {
    let mut out = String::from(
        "*Configurable settings* (set with `--config <key> <value>`):\n",
    );
    for s in SETTINGS {
        out.push_str(&format!(
            "• `{}` _({})_ — {}\n",
            s.name, s.kind, s.description
        ));
    }
    out
}

/// Permissive bool parser that accepts the spellings users might type
/// in Slack: `true/false`, `1/0`, `on/off`, `yes/no`. Case-insensitive.
fn parse_bool(s: &str) -> Result<bool, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "on" | "yes" | "y" => Ok(true),
        "false" | "0" | "off" | "no" | "n" => Ok(false),
        other => Err(format!(
            "expected a boolean (true/false, 1/0, on/off, yes/no), got {other:?}"
        )),
    }
}

fn read_name(name: &Arc<Mutex<String>>) -> String {
    name.lock()
        .map(|g| g.clone())
        .unwrap_or_else(|_| "CliBridge".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (TuiRenderer, RuntimeSettings) {
        let renderer = TuiRenderer::new(80, 24);
        let settings = RuntimeSettings {
            anchor_refresh: 10,
            name: Arc::new(Mutex::new("test".to_string())),
        };
        (renderer, settings)
    }

    #[test]
    fn parse_bool_accepts_common_spellings() {
        for s in &["true", "TRUE", "1", "on", "yes", "y"] {
            assert!(parse_bool(s).unwrap(), "{s}");
        }
        for s in &["false", "FALSE", "0", "off", "no", "n"] {
            assert!(!parse_bool(s).unwrap(), "{s}");
        }
        assert!(parse_bool("maybe").is_err());
    }

    #[test]
    fn apply_then_read_roundtrip_bool() {
        let (mut r, mut s) = fixture();
        apply_setting("replace_block_chars", "true", &mut r, &mut s).unwrap();
        assert_eq!(read_setting("replace_block_chars", &r, &s).unwrap(), "true");
        apply_setting("replace_block_chars", "off", &mut r, &mut s).unwrap();
        assert_eq!(read_setting("replace_block_chars", &r, &s).unwrap(), "false");
    }

    #[test]
    fn apply_then_read_roundtrip_int() {
        let (mut r, mut s) = fixture();
        apply_setting("anchor_refresh", "5", &mut r, &mut s).unwrap();
        assert_eq!(read_setting("anchor_refresh", &r, &s).unwrap(), "5");
        apply_setting("scroll_buffer", "200", &mut r, &mut s).unwrap();
        assert_eq!(read_setting("scroll_buffer", &r, &s).unwrap(), "200");
    }

    #[test]
    fn apply_name_trims_and_rejects_empty() {
        let (mut r, mut s) = fixture();
        apply_setting("name", "  hello  ", &mut r, &mut s).unwrap();
        assert_eq!(read_setting("name", &r, &s).unwrap(), "hello");
        assert!(apply_setting("name", "   ", &mut r, &mut s).is_err());
    }

    #[test]
    fn unknown_key_errors() {
        let (mut r, mut s) = fixture();
        let err = apply_setting("nope", "x", &mut r, &mut s).unwrap_err();
        assert!(err.contains("unknown setting"));
        assert!(read_setting("nope", &r, &s).is_none());
    }

    #[test]
    fn list_includes_every_setting() {
        let (r, s) = fixture();
        let out = list_settings(&r, &s);
        for setting in SETTINGS {
            assert!(out.contains(setting.name), "missing {}", setting.name);
        }
    }

    #[test]
    fn help_text_includes_every_setting() {
        let out = help_text();
        for setting in SETTINGS {
            assert!(out.contains(setting.name));
            assert!(out.contains(setting.description));
        }
    }
}
