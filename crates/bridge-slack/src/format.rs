//! Slack-specific transcript formatting for the async dispatcher.
//!
//! Implements [`TranscriptFormat`] for Slack's quirks:
//! - **Size unit is UTF-16 code units**, not bytes or `char`s — Slack's
//!   `msg_too_long` check counts UTF-16, and supplementary-plane emoji
//!   (📜 🟢 🌉) are one `char` but two units. Measuring in `char`s lets
//!   emoji-heavy bodies sneak past the limit, after which Slack silently
//!   truncates the edit mid-content with no closing fence.
//! - **Per-message ceiling well under the API cap.** Slack's desktop client
//!   collapses the tail of any text field over ~3,000 chars into a separate
//!   "Show more" bubble, which breaks fenced-code formatting (the closing
//!   code fence lands in the second bubble). We target a budget under that so
//!   each message renders as a single bubble, with headroom for labels.
//! - **Fenced code blocks** wrap scrollback rows.

use bridge_core::dispatch::TranscriptFormat;

/// Per-message size budget in UTF-16 code units. See module docs for the two
/// Slack ceilings this stays under. Mirrors the value the bridge used when it
/// posted scroll-buffer messages inline.
const SLACK_MESSAGE_SIZE_LIMIT: usize = 2_800;

const SCROLL_LABEL: &str = "📜 *Scroll buffer*";
const HISTORY_LABEL: &str = "📚 *History*";

/// Slack implementation of the dispatcher's formatting hooks.
pub struct SlackTranscriptFormat;

impl TranscriptFormat for SlackTranscriptFormat {
    fn size_limit(&self) -> usize {
        SLACK_MESSAGE_SIZE_LIMIT
    }

    fn measure(&self, s: &str) -> usize {
        s.encode_utf16().count()
    }

    fn scroll_body(&self, rows_text: &str) -> String {
        format!("{SCROLL_LABEL}\n```\n{rows_text}```")
    }

    fn lock_body(&self, active_body: &str) -> String {
        // Same inner rows, relabeled from Scroll buffer → History.
        active_body.replacen(SCROLL_LABEL, HISTORY_LABEL, 1)
    }

    fn extend_body(&self, existing: &str, new_rows_text: &str) -> String {
        // Body shape: "<label>\n```\n<rows>```". Splice new rows immediately
        // before the trailing fence.
        let trimmed = existing.strip_suffix("```").unwrap_or(existing);
        format!("{trimmed}{new_rows_text}```")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_utf16_units() {
        let f = SlackTranscriptFormat;
        assert_eq!(f.measure("hello"), 5);
        // 📜 is 1 char / 2 UTF-16 units.
        assert_eq!(f.measure("📜"), 2);
    }

    #[test]
    fn scroll_body_wraps_in_fence() {
        let f = SlackTranscriptFormat;
        assert_eq!(
            f.scroll_body("a\nb\n"),
            "📜 *Scroll buffer*\n```\na\nb\n```"
        );
    }

    #[test]
    fn lock_body_relabels_to_history() {
        let f = SlackTranscriptFormat;
        let active = f.scroll_body("row\n");
        assert_eq!(f.lock_body(&active), "📚 *History*\n```\nrow\n```");
    }

    #[test]
    fn extend_body_appends_before_fence() {
        let f = SlackTranscriptFormat;
        let body = f.scroll_body("first\n");
        let extended = f.extend_body(&body, "second\n");
        assert_eq!(extended, "📜 *Scroll buffer*\n```\nfirst\nsecond\n```");
    }

    #[test]
    fn extend_body_round_trips() {
        let f = SlackTranscriptFormat;
        let mut body = f.scroll_body("a\n");
        body = f.extend_body(&body, "b\n");
        body = f.extend_body(&body, "c\n");
        assert_eq!(body, "📜 *Scroll buffer*\n```\na\nb\nc\n```");
    }

    #[test]
    fn fresh_overhead_matches_empty_body() {
        let f = SlackTranscriptFormat;
        // Default impl measures scroll_body(""). Make sure it's the UTF-16
        // size of the header + empty fence (label has one 2-unit emoji).
        assert_eq!(f.fresh_overhead(), f.measure("📜 *Scroll buffer*\n```\n```"));
    }
}
