//! Slack-specific transcript formatting for the async dispatcher.
//!
//! Implements [`TranscriptFormat`] for Slack's quirks:
//! - **Size unit is UTF-16 code units**, not bytes or `char`s — Slack's
//!   `msg_too_long` check counts UTF-16, and supplementary-plane emoji
//!   (📜 🟢 🌉) are one `char` but two units. Measuring in `char`s lets
//!   emoji-heavy bodies sneak past the limit, after which Slack silently
//!   truncates the edit mid-content with no closing fence.
//! - **Slack escapes `&`, `<`, `>`** in the message text and counts the
//!   *escaped* length against `msg_too_long`. A body full of `<`, `>`, `&`
//!   (code, diffs, shell redirects — exactly what a dev CLI emits) grows when
//!   stored: each `<`/`>` → `&lt;`/`&gt;` (+3 units), each `&` → `&amp;` (+4).
//!   `measure` must count the escaped width or we send bodies we think fit but
//!   Slack rejects — which also wedges a scroll buffer that can never seal.
//! - **The self-marker is prepended at send time.** Every posted body gets a
//!   leading `🌉 ` (`SELF_MARKER`) added by the client *after* the dispatcher
//!   sized it, so the limit must reserve that width too.
//! - **Per-message ceiling well under the API cap.** Slack's desktop client
//!   collapses the tail of any text field over ~3,000 chars into a separate
//!   "Show more" bubble, which breaks fenced-code formatting (the closing
//!   code fence lands in the second bubble). We target a budget under that so
//!   each message renders as a single bubble, with headroom for labels.
//! - **Fenced code blocks** wrap scrollback rows.

use bridge_core::dispatch::{TranscriptFormat, shrink_by_chars};

/// Effective per-message budget in UTF-16 units. Slack's hard `msg_too_long`
/// ceiling (and its "Show more" bubble split) sit around 3,000; we stay under
/// at 2,800, then subtract the width of the self-marker the client prepends to
/// every body *after* the dispatcher has sized it (`🌉 ` = 3 units; asserted
/// against the live [`crate::client::SELF_MARKER`] in tests). So a body that
/// passes `measure` still fits once marked and entity-escaped.
const SLACK_MESSAGE_SIZE_LIMIT: usize = 2_800 - 3;

const SCROLL_LABEL: &str = "📜 *Scroll buffer*";
const HISTORY_LABEL: &str = "📚 *History*";

/// Slack implementation of the dispatcher's formatting hooks.
pub struct SlackTranscriptFormat;

impl TranscriptFormat for SlackTranscriptFormat {
    fn size_limit(&self) -> usize {
        SLACK_MESSAGE_SIZE_LIMIT
    }

    fn measure(&self, s: &str) -> usize {
        // Count what Slack actually stores: `&`, `<`, `>` are escaped to
        // `&amp;`, `&lt;`, `&gt;`, and msg_too_long is measured against that
        // escaped text. Count UTF-16 units (Slack's unit), charging the escape
        // expansion for those three characters.
        s.chars().map(slack_char_cost).sum()
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

    fn paginate(&self, body: &str) -> Vec<String> {
        if self.measure(body) <= SLACK_MESSAGE_SIZE_LIMIT {
            return vec![body.to_string()];
        }
        match split_fenced(body) {
            Some((header, content)) => self.paginate_fenced(header, content),
            None => self.paginate_plain(body),
        }
    }

    fn shrink(&self, body: &str, target: usize) -> Option<String> {
        // For a fenced body, trim from the *end of the content* and re-close
        // the fence so the result stays well-formed, until the whole framed
        // body measures at most `target`. The header and the start of the
        // content (oldest scrollback) are preserved; the tail is dropped — and
        // the dispatcher tracks the shrunk body as the new active, so the
        // dropped tail isn't re-counted on the next extend.
        match split_fenced(body) {
            Some((header, content)) => {
                // Budget for content = target minus the fixed framing overhead.
                let overhead =
                    self.measure(header) + self.measure(FENCE_OPEN) + self.measure(FENCE_CLOSE);
                let content_target = target.saturating_sub(overhead);
                // Trim content to fit, measured with the same (escape-aware)
                // accounting the rest of the format uses.
                let trimmed = shrink_by_chars(content, content_target, |s| self.measure(s))?;
                let framed = format!("{header}{FENCE_OPEN}{trimmed}{FENCE_CLOSE}");
                // Must end up strictly smaller than the input.
                if self.measure(&framed) < self.measure(body) {
                    Some(framed)
                } else {
                    None
                }
            }
            // Unfenced (plain reply / banner): trim raw chars from the end.
            None => shrink_by_chars(body, target, |s| self.measure(s)),
        }
    }
}

/// Closing code fence plus the bytes Slack adds around it.
const FENCE_OPEN: &str = "```\n";
const FENCE_CLOSE: &str = "```";

impl SlackTranscriptFormat {
    /// Paginate a fenced body. Each page repeats `header` (if any) and is
    /// wrapped in its own code fence so it renders as a self-contained,
    /// well-formed message. Splits on line boundaries; a single line wider
    /// than a whole page is hard-split at a UTF-16 boundary.
    fn paginate_fenced(&self, header: &str, content: &str) -> Vec<String> {
        // Per-page room for content = limit − (header + open fence + close
        // fence). `header` already includes its trailing newline when present.
        let overhead = self.measure(header) + self.measure(FENCE_OPEN) + self.measure(FENCE_CLOSE);
        let budget = SLACK_MESSAGE_SIZE_LIMIT.saturating_sub(overhead).max(1);

        let mut pages = Vec::new();
        let mut cur = String::new();
        let mut cur_size = 0usize;

        let flush = |cur: &mut String, cur_size: &mut usize, pages: &mut Vec<String>| {
            if !cur.is_empty() {
                pages.push(format!("{header}{FENCE_OPEN}{cur}{FENCE_CLOSE}"));
                cur.clear();
                *cur_size = 0;
            }
        };

        // `content` is the inner text (each source line ends in '\n'). Keep
        // the trailing newline on each line so fence formatting round-trips.
        for line in split_keep_newlines(content) {
            let line_size = self.measure(&line);
            if line_size > budget {
                // Single line too wide for a page: flush what we have, then
                // hard-split the line across pages at UTF-16 boundaries.
                flush(&mut cur, &mut cur_size, &mut pages);
                for chunk in self.hard_split(&line, budget) {
                    pages.push(format!("{header}{FENCE_OPEN}{chunk}{FENCE_CLOSE}"));
                }
                continue;
            }
            if cur_size + line_size > budget {
                flush(&mut cur, &mut cur_size, &mut pages);
            }
            cur.push_str(&line);
            cur_size += line_size;
        }
        flush(&mut cur, &mut cur_size, &mut pages);

        if pages.is_empty() {
            // Degenerate (empty content): preserve a well-formed empty body.
            pages.push(format!("{header}{FENCE_OPEN}{FENCE_CLOSE}"));
        }
        pages
    }

    /// Paginate plain (unfenced) text — e.g. a long command reply. Splits on
    /// line boundaries, hard-splitting any over-long line.
    fn paginate_plain(&self, body: &str) -> Vec<String> {
        let budget = SLACK_MESSAGE_SIZE_LIMIT.max(1);
        let mut pages = Vec::new();
        let mut cur = String::new();
        let mut cur_size = 0usize;
        for line in split_keep_newlines(body) {
            let line_size = self.measure(&line);
            if line_size > budget {
                if !cur.is_empty() {
                    pages.push(std::mem::take(&mut cur));
                    cur_size = 0;
                }
                pages.extend(self.hard_split(&line, budget));
                continue;
            }
            if cur_size + line_size > budget && !cur.is_empty() {
                pages.push(std::mem::take(&mut cur));
                cur_size = 0;
            }
            cur.push_str(&line);
            cur_size += line_size;
        }
        if !cur.is_empty() {
            pages.push(cur);
        }
        pages
    }

    /// Split `s` into pieces each measuring `<= budget` (escaped) units, at
    /// char boundaries so we never produce invalid UTF-8. Uses the same
    /// escape-aware per-char cost as `measure`, so an entity-heavy line (all
    /// `<`/`>`/`&`) splits to fit Slack's stored size, not the raw size.
    fn hard_split(&self, s: &str, budget: usize) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut cur_size = 0usize;
        for c in s.chars() {
            let cs = slack_char_cost(c);
            if cur_size + cs > budget && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                cur_size = 0;
            }
            cur.push(c);
            cur_size += cs;
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        out
    }
}

/// UTF-16 width of `c` as Slack stores it: `&`/`<`/`>` are escaped to
/// `&amp;`/`&lt;`/`&gt;` and counted at their escaped length; everything else
/// is its plain UTF-16 width.
fn slack_char_cost(c: char) -> usize {
    match c {
        '&' => 5,       // &amp;
        '<' | '>' => 4, // &lt; / &gt;
        _ => c.len_utf16(),
    }
}

/// Decompose a fenced body `"<header>```\n<content>```"` into `(header,
/// content)`. `header` includes its trailing newline (or is empty when the
/// body opens directly with the fence). Returns `None` if `body` isn't a
/// single fenced block of this shape.
fn split_fenced(body: &str) -> Option<(&str, &str)> {
    let open = body.find(FENCE_OPEN)?;
    let header = &body[..open];
    let after_open = &body[open + FENCE_OPEN.len()..];
    let content = after_open.strip_suffix(FENCE_CLOSE)?;
    // Reject bodies with additional fences inside — those aren't the simple
    // single-block shape this splitter understands, so fall back to plain.
    if content.contains("```") {
        return None;
    }
    Some((header, content))
}

/// Split text into lines, each retaining its trailing '\n' (the last piece
/// keeps whatever it had). Empty input yields no pieces.
fn split_keep_newlines(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in s.chars() {
        cur.push(c);
        if c == '\n' {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
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
    fn measure_charges_slack_entity_escaping() {
        // Slack stores &/</> as &amp;/&lt;/&gt; and counts the escaped length.
        // We must charge the expansion so we never send a body Slack rejects.
        let f = SlackTranscriptFormat;
        assert_eq!(f.measure("<"), 4); // &lt;
        assert_eq!(f.measure(">"), 4); // &gt;
        assert_eq!(f.measure("&"), 5); // &amp;
        // A line of shell redirection: `cat <f >g && echo` — count expands.
        let raw = "a <b> c & d";
        // a(1) space(1) <(4) b(1) >(4) space(1) c(1) space(1) &(5) space(1) d(1)
        assert_eq!(f.measure(raw), 21);
        // Sanity: that's larger than the naive UTF-16 count (11).
        assert_eq!(raw.encode_utf16().count(), 11);
    }

    #[test]
    fn limit_reserves_room_for_self_marker() {
        // The effective budget is below Slack's raw ceiling by the marker width
        // the client prepends, so a body at the limit still fits once marked.
        // Assert against the live marker so a change to it can't silently
        // desync the budget.
        let f = SlackTranscriptFormat;
        let marker_units = crate::client::SELF_MARKER.encode_utf16().count();
        assert_eq!(f.size_limit(), 2_800 - marker_units);
    }

    #[test]
    fn shrink_trims_content_to_target_and_keeps_fence_closed() {
        // Shrinking a fenced body to a target drops content from the end but
        // leaves a well-formed message: header preserved, fence re-closed, and
        // the whole framed body measures at most the target.
        let f = SlackTranscriptFormat;
        let body = f.scroll_body(&"x".repeat(100));
        let before = f.measure(&body);
        let target = before - 30;
        let shrunk = f.shrink(&body, target).expect("should shrink");
        assert!(shrunk.starts_with("📜 *Scroll buffer*\n```\n"));
        assert!(shrunk.ends_with("```"));
        assert!(
            f.measure(&shrunk) <= target,
            "shrunk ({}) must be within target {target}",
            f.measure(&shrunk)
        );
        // Start of content preserved (we trim from the tail).
        assert!(shrunk.contains("xxxx"));
    }

    #[test]
    fn shrink_unfenced_trims_to_target() {
        let f = SlackTranscriptFormat;
        let body = "hello world this is a reply";
        let shrunk = f.shrink(body, 5).unwrap();
        assert!(shrunk.starts_with("hello"));
        assert!(f.measure(&shrunk) <= 5);
    }

    #[test]
    fn shrink_gives_up_when_framing_exceeds_target() {
        let f = SlackTranscriptFormat;
        // Target smaller than the fixed fence/header framing → can't fit.
        let body = f.scroll_body("some content");
        assert!(f.shrink(&body, 1).is_none());
    }

    #[test]
    fn entity_heavy_body_paginates_to_fit_escaped() {
        // Regression: a body of all '<' (each escapes to 4 units) that's "under
        // the limit" by naive count must still split so the *escaped* size of
        // every page fits. This is the kiro-cli wedge: code/diff output is full
        // of </>/& and was sailing past the size check, then Slack rejected the
        // edit and the scroll buffer could neither extend nor seal.
        let f = SlackTranscriptFormat;
        let line: String = "<".repeat(2_000); // 2000 raw, 8000 escaped units
        let body = format!("🟢 *Live*\n```\n{line}\n```");
        let pages = f.paginate(&body);
        assert!(
            pages.len() > 1,
            "entity-heavy body must split: {}",
            pages.len()
        );
        for page in &pages {
            assert!(
                f.measure(page) <= f.size_limit(),
                "page escaped-size {} over limit {}",
                f.measure(page),
                f.size_limit()
            );
        }
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
        assert_eq!(
            f.fresh_overhead(),
            f.measure("📜 *Scroll buffer*\n```\n```")
        );
    }

    #[test]
    fn paginate_passes_through_when_within_limit() {
        let f = SlackTranscriptFormat;
        let body = "🟢 *Live*\n```\nhello\nworld\n```";
        assert_eq!(f.paginate(body), vec![body.to_string()]);
    }

    #[test]
    fn paginate_splits_overlong_fenced_body() {
        let f = SlackTranscriptFormat;
        // Build a live frame far over the limit: 200 lines of 60 chars each
        // = ~12,200 units, well past 2,800.
        let mut content = String::new();
        for i in 0..200 {
            content.push_str(&format!("{:0>59}\n", i)); // 59 digits + '\n' = 60
        }
        let body = format!("🟢 *Live*\n```\n{content}```");
        let pages = f.paginate(&body);
        assert!(
            pages.len() > 1,
            "expected multiple pages, got {}",
            pages.len()
        );
        for (i, page) in pages.iter().enumerate() {
            // Every page is within the limit.
            assert!(
                f.measure(page) <= SLACK_MESSAGE_SIZE_LIMIT,
                "page {i} over limit: {} units",
                f.measure(page)
            );
            // Every page is a well-formed fenced block: header + open +
            // closing fence.
            assert!(page.starts_with("🟢 *Live*\n```\n"), "page {i}: {page:?}");
            assert!(page.ends_with("```"), "page {i} missing closing fence");
        }
        // No content lost: concatenated inner text equals the original.
        let rejoined: String = pages
            .iter()
            .map(|p| {
                let inner = p
                    .strip_prefix("🟢 *Live*\n```\n")
                    .unwrap()
                    .strip_suffix("```")
                    .unwrap();
                inner.to_string()
            })
            .collect();
        assert_eq!(rejoined, content);
    }

    #[test]
    fn paginate_hard_splits_a_single_overwide_line() {
        let f = SlackTranscriptFormat;
        // One line with no newlines, far wider than a page.
        let huge: String = "x".repeat(SLACK_MESSAGE_SIZE_LIMIT * 3);
        let body = format!("🟢 *Live*\n```\n{huge}```");
        let pages = f.paginate(&body);
        assert!(
            pages.len() >= 3,
            "expected hard-split across pages: {}",
            pages.len()
        );
        for page in &pages {
            assert!(f.measure(page) <= SLACK_MESSAGE_SIZE_LIMIT);
            assert!(page.ends_with("```"));
        }
    }

    #[test]
    fn paginate_plain_reply_splits_on_lines() {
        let f = SlackTranscriptFormat;
        // A long unfenced body (e.g. a giant --config listing).
        let mut body = String::new();
        for i in 0..1000 {
            body.push_str(&format!("setting_{i} = value\n"));
        }
        let pages = f.paginate(&body);
        assert!(pages.len() > 1);
        for page in &pages {
            assert!(f.measure(page) <= SLACK_MESSAGE_SIZE_LIMIT);
        }
        // No content lost.
        let rejoined: String = pages.concat();
        assert_eq!(rejoined, body);
    }

    #[test]
    fn paginate_preserves_emoji_boundaries() {
        let f = SlackTranscriptFormat;
        // A line of emoji (2 UTF-16 units each) that must hard-split without
        // cutting a codepoint.
        let line: String = "📜".repeat(SLACK_MESSAGE_SIZE_LIMIT);
        let body = format!("🟢 *Live*\n```\n{line}```");
        let pages = f.paginate(&body);
        for page in &pages {
            assert!(f.measure(page) <= SLACK_MESSAGE_SIZE_LIMIT);
            // Valid UTF-8 / no replacement chars from a bad split.
            assert!(!page.contains('\u{FFFD}'));
        }
    }
}
