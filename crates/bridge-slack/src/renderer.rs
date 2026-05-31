/// Renders terminal output for display in Slack messages.
///
/// Strategy:
/// - Accumulates terminal output as it arrives. `process()` does not render.
/// - Detects "TUI mode" only when an app enables the alternate screen buffer
///   (vim, htop, less, tmux). Plain output stays in streaming mode.
/// - Streaming mode: appends to a buffer that the bridge drains on a fixed
///   tick via `take_pending()` and posts as a new message.
/// - TUI mode: maintains a virtual screen and renders the full frame on tick;
///   the bridge edits the existing message.
pub struct TuiRenderer {
    /// Current screen buffer (rows x cols)
    screen: Vec<Vec<char>>,
    /// Terminal dimensions
    cols: usize,
    rows: usize,
    /// Cursor position
    cursor_row: usize,
    cursor_col: usize,
    /// Whether we've detected TUI-mode output
    tui_mode: bool,
    /// Accumulated streaming output, drained by `take_pending`.
    line_buffer: String,
    /// Streaming text left over when we transitioned into TUI mode mid-stream.
    /// Returned (and cleared) by the next `take_pending` so we don't lose the
    /// pre-TUI output. Held separately because once `tui_mode` is on, the next
    /// `take_pending` would otherwise return a TUI frame.
    pending_handoff: Option<String>,
    /// Bytes carried over from the previous `process()` call. ConPTY (and PTYs
    /// in general) chunk output without regard to escape-sequence or UTF-8
    /// boundaries — a chunk can end mid-`\x1b[1;4;2m`, mid-OSC, or mid-codepoint.
    /// We stash any incomplete trailing bytes here and prepend them to the next
    /// chunk so the parser only ever sees complete units.
    input_carry: Vec<u8>,
    /// Whether the TUI screen has changed since the last `take_pending`.
    dirty: bool,
}

impl TuiRenderer {
    pub fn new(cols: u16, rows: u16) -> Self {
        let cols = cols as usize;
        let rows = rows as usize;
        Self {
            screen: vec![vec![' '; cols]; rows],
            cols,
            rows,
            cursor_row: 0,
            cursor_col: 0,
            tui_mode: false,
            line_buffer: String::new(),
            pending_handoff: None,
            input_carry: Vec::new(),
            dirty: false,
        }
    }

    /// Append raw terminal output bytes to the renderer's buffer. Cheap and
    /// allocation-light; the bridge calls `take_pending()` on a tick to actually
    /// produce a message.
    pub fn process(&mut self, data: &[u8]) {
        // Combine carry from the previous chunk with this one. ConPTY likes
        // to slice in the middle of \x1b[1;4;2m and the like; without this
        // step we'd render fragments as literal "u1u4;2m" garbage.
        let mut bytes: Vec<u8> = Vec::with_capacity(self.input_carry.len() + data.len());
        bytes.extend_from_slice(&self.input_carry);
        bytes.extend_from_slice(data);
        self.input_carry.clear();

        let process_end = find_process_end(&bytes);
        if process_end < bytes.len() {
            self.input_carry.extend_from_slice(&bytes[process_end..]);
        }
        let to_process = &bytes[..process_end];
        if to_process.is_empty() {
            return;
        }

        let text = String::from_utf8_lossy(to_process);

        if !self.tui_mode && Self::is_tui_output(&text) {
            self.tui_mode = true;
            // Preserve whatever streaming text we'd already accumulated so
            // it gets posted before the first TUI frame.
            if !self.line_buffer.is_empty() {
                self.pending_handoff = Some(std::mem::take(&mut self.line_buffer));
            }
        }

        if self.tui_mode {
            self.process_tui(&text);
        } else {
            self.process_streaming(&text);
        }
    }

    /// Drain whatever output has accumulated since the last call.
    /// In streaming mode this empties the buffer; in TUI mode it returns
    /// the current screen and clears the dirty flag.
    pub fn take_pending(&mut self) -> Option<RenderedOutput> {
        // Drain any leftover streaming text that was buffered before we
        // transitioned into TUI mode. Always return it as a fresh post.
        if let Some(chunk) = self.pending_handoff.take() {
            return Some(RenderedOutput {
                text: format!("```\n{chunk}```"),
                is_edit: false,
            });
        }

        if self.tui_mode {
            if !self.dirty {
                return None;
            }
            self.dirty = false;
            Some(RenderedOutput {
                text: self.render_screen(),
                is_edit: true,
            })
        } else {
            if self.line_buffer.is_empty() {
                return None;
            }
            let chunk = std::mem::take(&mut self.line_buffer);
            Some(RenderedOutput {
                text: format!("```\n{chunk}```"),
                is_edit: false,
            })
        }
    }

    /// Resize the virtual screen.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let new_cols = cols as usize;
        let new_rows = rows as usize;
        self.screen.resize(new_rows, vec![' '; new_cols]);
        for row in &mut self.screen {
            row.resize(new_cols, ' ');
        }
        self.cols = new_cols;
        self.rows = new_rows;
        self.cursor_row = self.cursor_row.min(new_rows.saturating_sub(1));
        self.cursor_col = self.cursor_col.min(new_cols.saturating_sub(1));
    }

    /// Check if we're in TUI mode.
    pub fn is_tui_mode(&self) -> bool {
        self.tui_mode
    }

    fn is_tui_output(text: &str) -> bool {
        // Indicators a full-screen app is taking over the terminal:
        //  - \x1b[?1049h / ?47h: alternate screen buffer (vim, htop, less, tmux)
        //  - \x1b[2J:           full-screen clear (Claude Code, many TUIs)
        //  - \x1b[<r>;<c>H:     two-arg cursor positioning, used to lay out boxes
        //
        // We deliberately do NOT trigger on bare \x1b[H or \x1b[?25l: cmd.exe
        // and PowerShell emit those during normal prompt redraws on Windows
        // ConPTY, so those alone would mis-classify regular output as a TUI.
        if text.contains("\x1b[?1049h") || text.contains("\x1b[?47h") || text.contains("\x1b[2J") {
            return true;
        }
        contains_two_arg_cursor_position(text)
    }

    fn process_tui(&mut self, text: &str) {
        let mut chars = text.chars().peekable();

        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                match chars.peek() {
                    Some(&'[') => {
                        chars.next();
                        // CSI structure (ECMA-48):
                        //   parameter bytes:    0x30-0x3F  (0-9 : ; < = > ?)
                        //   intermediate bytes: 0x20-0x2F  (space ! " # $ % & ' ( ) * + , - . /)
                        //   final byte:         0x40-0x7E  (@-~)
                        // We were previously only accepting digits, ';' and '?',
                        // so e.g. Kitty's \x1b[>1u (with '>' as a parameter
                        // byte) terminated parsing early at '>', and "1u" then
                        // rendered as literal text.
                        let mut params = String::new();
                        while let Some(&c) = chars.peek() {
                            let cu = c as u32;
                            if (0x30..=0x3F).contains(&cu) {
                                params.push(c);
                                chars.next();
                            } else {
                                break;
                            }
                        }
                        // Skip optional intermediate bytes (we don't act on them).
                        while let Some(&c) = chars.peek() {
                            let cu = c as u32;
                            if (0x20..=0x2F).contains(&cu) {
                                chars.next();
                            } else {
                                break;
                            }
                        }
                        if let Some(&cmd) = chars.peek() {
                            chars.next();
                            self.handle_csi(&params, cmd);
                        }
                    }
                    Some(&']') | Some(&'P') | Some(&'X') | Some(&'^') | Some(&'_') => {
                        // String escapes: OSC (]), DCS (P), SOS (X), PM (^),
                        // APC (_). All share the BEL or ST terminator.
                        chars.next();
                        skip_string_body(&mut chars);
                    }
                    Some(_) => {
                        // Single-char escapes (e.g. \x1b=, \x1b>, \x1bM). Drop
                        // the next char so it doesn't render as a literal.
                        chars.next();
                    }
                    None => {}
                }
            } else {
                self.put_char(ch);
            }
        }
        self.dirty = true;
    }

    fn process_streaming(&mut self, text: &str) {
        // Strip ANSI escape sequences for streaming mode
        let clean = strip_ansi(text);
        self.line_buffer.push_str(&clean);
        self.dirty = true;
    }

    fn handle_csi(&mut self, params: &str, cmd: char) {
        // Strip a leading private-marker byte if present (?, <, >, =) so the
        // numeric arg parses cleanly. We don't actually act on private CSIs;
        // this just keeps `nums` from absorbing an empty entry that would
        // shift the indices below.
        let body = params.trim_start_matches(['?', '<', '>', '=']);
        let nums: Vec<usize> = body
            .split(';')
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect();

        match cmd {
            'H' | 'f' => {
                // Cursor position (row;col) — 1-indexed
                let row = nums.first().copied().unwrap_or(1).saturating_sub(1);
                let col = nums.get(1).copied().unwrap_or(1).saturating_sub(1);
                self.cursor_row = row.min(self.rows - 1);
                self.cursor_col = col.min(self.cols - 1);
            }
            'A' => {
                // Cursor up
                let n = nums.first().copied().unwrap_or(1);
                self.cursor_row = self.cursor_row.saturating_sub(n);
            }
            'B' => {
                // Cursor down
                let n = nums.first().copied().unwrap_or(1);
                self.cursor_row = (self.cursor_row + n).min(self.rows - 1);
            }
            'C' => {
                // Cursor forward
                let n = nums.first().copied().unwrap_or(1);
                self.cursor_col = (self.cursor_col + n).min(self.cols - 1);
            }
            'D' => {
                // Cursor back
                let n = nums.first().copied().unwrap_or(1);
                self.cursor_col = self.cursor_col.saturating_sub(n);
            }
            'J' => {
                // Erase in display
                let mode = nums.first().copied().unwrap_or(0);
                match mode {
                    2 | 3 => {
                        // Clear entire screen
                        for row in &mut self.screen {
                            row.fill(' ');
                        }
                    }
                    0 => {
                        // Clear from cursor to end
                        self.screen[self.cursor_row][self.cursor_col..].fill(' ');
                        for row in &mut self.screen[self.cursor_row + 1..] {
                            row.fill(' ');
                        }
                    }
                    1 => {
                        // Clear from start to cursor
                        for row in &mut self.screen[..self.cursor_row] {
                            row.fill(' ');
                        }
                        self.screen[self.cursor_row][..=self.cursor_col].fill(' ');
                    }
                    _ => {}
                }
            }
            'K' => {
                // Erase in line
                let mode = nums.first().copied().unwrap_or(0);
                match mode {
                    0 => self.screen[self.cursor_row][self.cursor_col..].fill(' '),
                    1 => self.screen[self.cursor_row][..=self.cursor_col].fill(' '),
                    2 => self.screen[self.cursor_row].fill(' '),
                    _ => {}
                }
            }
            'm' => {
                // SGR (colors/attributes) — we ignore these for text rendering
            }
            _ => {}
        }
    }

    fn put_char(&mut self, ch: char) {
        match ch {
            '\n' => {
                self.cursor_row += 1;
                if self.cursor_row >= self.rows {
                    // Scroll up
                    self.screen.remove(0);
                    self.screen.push(vec![' '; self.cols]);
                    self.cursor_row = self.rows - 1;
                }
            }
            '\r' => {
                self.cursor_col = 0;
            }
            '\x08' => {
                // Backspace
                self.cursor_col = self.cursor_col.saturating_sub(1);
            }
            '\t' => {
                // Tab — advance to next 8-column boundary
                let next_tab = (self.cursor_col / 8 + 1) * 8;
                self.cursor_col = next_tab.min(self.cols - 1);
            }
            c if !c.is_control() && self.cursor_col < self.cols && self.cursor_row < self.rows => {
                self.screen[self.cursor_row][self.cursor_col] = c;
                self.cursor_col += 1;
                if self.cursor_col >= self.cols {
                    self.cursor_col = 0;
                    self.cursor_row += 1;
                    if self.cursor_row >= self.rows {
                        self.screen.remove(0);
                        self.screen.push(vec![' '; self.cols]);
                        self.cursor_row = self.rows - 1;
                    }
                }
            }
            _ => {}
        }
    }

    fn render_screen(&self) -> String {
        let mut output = String::from("```\n");
        for row in &self.screen {
            let line: String = row.iter().collect();
            output.push_str(line.trim_end());
            output.push('\n');
        }
        output.push_str("```");
        output
    }
}

/// Output from the renderer, indicating whether to post a new message or edit existing.
#[derive(Debug, Clone)]
pub struct RenderedOutput {
    /// The formatted text to send/update
    pub text: String,
    /// If true, edit the previous message. If false, post a new one.
    pub is_edit: bool,
}

/// Walk `bytes` and return the offset at which the last *complete* unit ends.
/// A unit is one of: a UTF-8 codepoint, a CSI sequence (`\x1b[...<final>`),
/// an OSC sequence (`\x1b]...<terminator>`), or a two-byte ESC sequence.
/// Bytes from the returned offset to the end should be carried over to the
/// next chunk so the parser never sees a half-finished sequence.
fn find_process_end(bytes: &[u8]) -> usize {
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];

        if b == 0x1b {
            // ESC alone — wait for follow-up byte.
            let Some(&next) = bytes.get(i + 1) else {
                return i;
            };
            match next {
                b'[' => {
                    // CSI: zero or more parameter bytes (0x30-0x3F) and
                    // intermediate bytes (0x20-0x2F), then a final (0x40-0x7E).
                    let mut j = i + 2;
                    let mut found_final = false;
                    while let Some(&c) = bytes.get(j) {
                        if (0x40..=0x7E).contains(&c) {
                            j += 1;
                            found_final = true;
                            break;
                        }
                        if (0x20..=0x3F).contains(&c) {
                            j += 1;
                        } else {
                            // Anything else aborts the sequence — treat what
                            // we've seen as complete so the loop advances.
                            found_final = true;
                            break;
                        }
                    }
                    if !found_final {
                        // Ran out of bytes before the final byte arrived —
                        // carry from the ESC.
                        return i;
                    }
                    i = j;
                }
                // String escapes — all share the same ST-terminated body.
                // OSC accepts BEL (0x07) too; the rest are technically ST-only
                // but we accept BEL there too for robustness against terminals
                // that conflate them.
                b']' | b'P' | b'X' | b'^' | b'_' => match find_string_terminator(bytes, i + 2) {
                    Some(end) => i = end,
                    None => return i,
                },
                _ => {
                    // Two-byte ESC sequence: \x1b followed by one byte.
                    i += 2;
                }
            }
            continue;
        }

        // Regular byte. Check for a partial UTF-8 codepoint at the tail.
        if b < 0x80 {
            i += 1;
        } else {
            let need = utf8_seq_len(b);
            if need == 0 {
                // Invalid leading byte — pass it through so from_utf8_lossy
                // turns it into U+FFFD.
                i += 1;
            } else if i + need <= bytes.len() {
                i += need;
            } else {
                // Incomplete codepoint at the tail.
                return i;
            }
        }
    }
    i
}

/// Length in bytes of the UTF-8 sequence starting with `lead`, or 0 if `lead`
/// is not a valid leading byte.
fn utf8_seq_len(lead: u8) -> usize {
    match lead {
        0x00..=0x7F => 1,
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => 0,
    }
}

/// Scan forward from `start` for a String-Terminator: BEL (0x07) or
/// ST (`\x1b\\`). Returns the index *just past* the terminator if found,
/// or `None` if the terminator hasn't arrived yet (i.e. the body is still
/// incomplete and the caller should wait for more bytes).
///
/// Used for OSC (`\x1b]`), DCS (`\x1bP`), SOS (`\x1bX`), PM (`\x1b^`),
/// and APC (`\x1b_`) — they all share the same body shape.
fn find_string_terminator(bytes: &[u8], start: usize) -> Option<usize> {
    let mut j = start;
    while j < bytes.len() {
        let c = bytes[j];
        if c == 0x07 {
            return Some(j + 1);
        }
        if c == 0x1b {
            let &after = bytes.get(j + 1)?;
            if after == b'\\' {
                return Some(j + 2);
            }
            // Nested ESC inside the string body. xterm treats this as a
            // hard terminator (resync) — do the same so we don't get stuck.
            return Some(j + 1);
        }
        j += 1;
    }
    None
}

/// Strip ANSI escape sequences from text. Handles:
///  - CSI: `\x1b[...<final>` where `<final>` is an ASCII letter
///  - String escapes (OSC `\x1b]`, DCS `\x1bP`, SOS `\x1bX`, PM `\x1b^`,
///    APC `\x1b_`): body terminated by BEL (`\x07`) or ST (`\x1b\\`)
///  - Single-char escapes: `\x1b<X>` for any other introducer
///  - Stray BEL (`\x07`), so it can't leak into rendered output
fn strip_ansi(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => match chars.peek() {
                Some(&'[') => {
                    chars.next();
                    // CSI body: parameter bytes (0x30-0x3F), intermediate
                    // bytes (0x20-0x2F), then a final byte (0x40-0x7E).
                    while let Some(&c) = chars.peek() {
                        chars.next();
                        if (0x40..=0x7E).contains(&(c as u32)) {
                            break;
                        }
                    }
                }
                Some(&']') | Some(&'P') | Some(&'X') | Some(&'^') | Some(&'_') => {
                    // String escapes share the same body: scan to BEL or ST.
                    // OSC (]), DCS (P), SOS (X), PM (^), APC (_).
                    chars.next();
                    skip_string_body(&mut chars);
                }
                Some(_) => {
                    // Two-byte escape (\x1b=, \x1b>, \x1bM, etc.) — drop both.
                    chars.next();
                }
                None => {}
            },
            '\x07' => {
                // Stray BEL — drop. We already swallow it as the OSC terminator
                // above, but ConPTY occasionally emits it on its own.
            }
            _ => result.push(ch),
        }
    }

    result
}

/// Consume an OSC/DCS/SOS/PM/APC body up to and including its terminator.
/// Body ends at BEL (0x07) or ST (`\x1b\\`). A nested ESC without a `\`
/// after it acts as a hard terminator (xterm-style resync).
fn skip_string_body<I: Iterator<Item = char>>(chars: &mut std::iter::Peekable<I>) {
    while let Some(c) = chars.next() {
        if c == '\x07' {
            return;
        }
        if c == '\x1b' {
            if chars.peek() == Some(&'\\') {
                chars.next();
            }
            return;
        }
    }
}

/// Heuristic: does `text` contain a CSI cursor-position escape with at least
/// one explicit parameter (e.g. `\x1b[3;5H`)? Bare `\x1b[H` is excluded because
/// cmd.exe uses it for prompt redraws. Apps that lay out boxes via absolute
/// positioning (Claude Code, ncurses-style TUIs) emit the parameterized form.
fn contains_two_arg_cursor_position(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == 0x1b && bytes[i + 1] == b'[' {
            let mut j = i + 2;
            let mut saw_digit = false;
            while j < bytes.len() {
                let b = bytes[j];
                if b.is_ascii_digit() || b == b';' {
                    if b.is_ascii_digit() {
                        saw_digit = true;
                    }
                    j += 1;
                } else {
                    if saw_digit && (b == b'H' || b == b'f') {
                        return true;
                    }
                    break;
                }
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_streaming_output() {
        let mut renderer = TuiRenderer::new(80, 24);
        renderer.process(b"hello world\n");
        let output = renderer.take_pending().unwrap();
        assert!(!output.is_edit);
        assert!(output.text.contains("hello world"));
    }

    #[test]
    fn test_streaming_drains_buffer() {
        // Two `process` calls accumulate; one `take_pending` drains; the next
        // `take_pending` returns None until more data arrives.
        let mut renderer = TuiRenderer::new(80, 24);
        renderer.process(b"first\n");
        renderer.process(b"second\n");
        let out = renderer.take_pending().unwrap();
        assert!(out.text.contains("first"));
        assert!(out.text.contains("second"));
        assert!(renderer.take_pending().is_none());
        renderer.process(b"third\n");
        assert!(renderer.take_pending().unwrap().text.contains("third"));
    }

    #[test]
    fn test_tui_detection() {
        // Triggers TUI mode:
        assert!(TuiRenderer::is_tui_output("\x1b[?1049h"));
        assert!(TuiRenderer::is_tui_output("\x1b[?47h"));
        assert!(TuiRenderer::is_tui_output("\x1b[2J"));
        assert!(TuiRenderer::is_tui_output("\x1b[3;5Hx"));
        // Does NOT trigger (cmd.exe / PowerShell prompt echo):
        assert!(!TuiRenderer::is_tui_output("\x1b[H"));
        assert!(!TuiRenderer::is_tui_output("\x1b[?25l"));
        assert!(!TuiRenderer::is_tui_output("\x1b[?25l\x1b[H"));
        assert!(!TuiRenderer::is_tui_output("plain text"));
    }

    #[test]
    fn test_cmd_echo_stays_streaming() {
        let mut renderer = TuiRenderer::new(80, 24);
        // Typical ConPTY prompt redraw: hide cursor, home, then text.
        renderer.process(b"\x1b[?25l\x1b[Hhello\r\n");
        assert!(!renderer.is_tui_mode());
        let out = renderer.take_pending().unwrap();
        assert!(!out.is_edit);
        assert!(out.text.contains("hello"));
    }

    #[test]
    fn test_tui_mode_renders_screen() {
        let mut renderer = TuiRenderer::new(10, 3);
        // Enable alt screen + write something.
        renderer.process(b"\x1b[?1049h\x1b[2J\x1b[1;1Hhello");
        let output = renderer.take_pending().unwrap();
        assert!(output.is_edit);
        assert!(output.text.contains("hello"));
    }

    #[test]
    fn test_strip_ansi() {
        assert_eq!(strip_ansi("\x1b[32mgreen\x1b[0m"), "green");
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn test_strip_ansi_osc_window_title() {
        // cmd.exe sets the window title via OSC 0: \x1b]0;TITLE\x07
        assert_eq!(
            strip_ansi("before\x1b]0;C:\\Windows\\system32\\cmd.exe\x07after"),
            "beforeafter"
        );
        // ST-terminated form
        assert_eq!(strip_ansi("a\x1b]0;title\x1b\\b"), "ab");
    }

    #[test]
    fn test_strip_ansi_stray_bel() {
        assert_eq!(strip_ansi("hi\x07there"), "hithere");
    }

    #[test]
    fn test_strip_ansi_two_byte_escape() {
        // \x1b= and \x1b> are application-keypad escapes; both bytes drop.
        assert_eq!(strip_ansi("a\x1b=b\x1b>c"), "abc");
    }

    #[test]
    fn test_streaming_drops_window_title() {
        // Regression: dir output used to leak the OSC body and BEL into Slack.
        let mut renderer = TuiRenderer::new(80, 24);
        renderer.process(b"line1\n\x1b]0;C:\\WINDOWS\\system32\\cmd.exe\x07line2\n");
        let out = renderer.take_pending().unwrap();
        assert!(!out.is_edit);
        assert!(out.text.contains("line1"));
        assert!(out.text.contains("line2"));
        assert!(!out.text.contains(']'));
        assert!(!out.text.contains('\x07'));
        assert!(!out.text.contains("cmd.exe"));
    }

    #[test]
    fn test_two_arg_cursor_position_detected() {
        // Apps that lay out boxes use parameterized H, e.g. claude-code.
        assert!(contains_two_arg_cursor_position("\x1b[3;5Hhi"));
        assert!(contains_two_arg_cursor_position("\x1b[10Hbar"));
        assert!(contains_two_arg_cursor_position("\x1b[2;1f"));
        // Bare \x1b[H is cmd.exe's prompt redraw — must NOT trigger TUI mode.
        assert!(!contains_two_arg_cursor_position("\x1b[H"));
        assert!(!contains_two_arg_cursor_position("\x1b[?25l\x1b[H"));
        assert!(!contains_two_arg_cursor_position("plain"));
    }

    #[test]
    fn test_tui_mode_triggers_on_screen_clear() {
        // \x1b[2J alone is enough — Claude Code uses it to (re)paint.
        let mut renderer = TuiRenderer::new(20, 5);
        renderer.process(b"\x1b[2J\x1b[1;1Hhi");
        assert!(renderer.is_tui_mode());
    }

    #[test]
    fn test_strip_ansi_dcs() {
        // DCS body — apps use this for capability negotiation. Used to leak
        // through as literal "u1u4;2m" garbage in the rendered output.
        assert_eq!(strip_ansi("a\x1bP1u\x1b\\b"), "ab");
        assert_eq!(strip_ansi("a\x1bP$qm\x1b\\b"), "ab"); // DECRQSS query
        // BEL-terminated form
        assert_eq!(strip_ansi("a\x1bP1u\x07b"), "ab");
    }

    #[test]
    fn test_strip_ansi_apc_pm_sos() {
        // All string escapes share the same body shape.
        assert_eq!(strip_ansi("a\x1b_kitty stuff\x1b\\b"), "ab"); // APC
        assert_eq!(strip_ansi("a\x1b^private msg\x1b\\b"), "ab"); // PM
        assert_eq!(strip_ansi("a\x1bXstart of string\x1b\\b"), "ab"); // SOS
    }

    #[test]
    fn test_tui_consumes_kitty_keyboard_escape() {
        // Regression: Claude Code uses Kitty's keyboard protocol, which emits
        // \x1b[>1u and \x1b[<u. The TUI parser used to stop at the '>' and
        // render "1u" as literal text in the screen buffer.
        let mut renderer = TuiRenderer::new(20, 3);
        // Force TUI mode without otherwise touching the screen.
        renderer.process(b"\x1b[2J\x1b[1;1H");
        renderer.process(b"\x1b[>1u\x1b[<uhello");
        let out = renderer.take_pending().unwrap();
        assert!(out.is_edit);
        assert!(out.text.contains("hello"));
        assert!(!out.text.contains("1u"));
        assert!(!out.text.contains(">1"));
        assert!(!out.text.contains('<'));
    }

    #[test]
    fn test_streaming_drops_dcs() {
        // Regression for `u1u4;2m` leaking into Claude Code output.
        let mut renderer = TuiRenderer::new(80, 24);
        renderer.process(b"hello \x1bP1u\x1b\\world\n");
        let out = renderer.take_pending().unwrap();
        assert!(out.text.contains("hello "));
        assert!(out.text.contains("world"));
        assert!(!out.text.contains("u1u"));
        assert!(!out.text.contains('P'));
    }

    #[test]
    fn test_carry_split_dcs() {
        // ConPTY can split DCS the same way it splits CSI.
        let mut renderer = TuiRenderer::new(80, 24);
        renderer.process(b"hello \x1bP1u");
        renderer.process(b"\x1b\\world\n");
        let out = renderer.take_pending().unwrap();
        assert!(out.text.contains("hello "));
        assert!(out.text.contains("world"));
        assert!(!out.text.contains("u1u"));
    }

    #[test]
    fn test_carry_split_csi() {
        // ConPTY chunks output without regard to escape boundaries. A split
        // mid-CSI must NOT leak into the rendered text as literal bytes.
        let mut renderer = TuiRenderer::new(80, 24);
        renderer.process(b"hello \x1b[1");
        renderer.process(b";4;2mworld\n");
        let out = renderer.take_pending().unwrap();
        assert!(out.text.contains("hello "));
        assert!(out.text.contains("world"));
        assert!(!out.text.contains("u1u4"));
        assert!(!out.text.contains(';'));
        assert!(!out.text.contains("[1"));
    }

    #[test]
    fn test_carry_lone_esc_at_chunk_end() {
        let mut renderer = TuiRenderer::new(80, 24);
        renderer.process(b"abc\x1b");
        renderer.process(b"[31mred\x1b[0m\n");
        let out = renderer.take_pending().unwrap();
        assert!(out.text.contains("abc"));
        assert!(out.text.contains("red"));
        assert!(!out.text.contains('['));
    }

    #[test]
    fn test_carry_split_osc() {
        // OSC body split across chunks (cmd.exe sends \x1b]0;TITLE\x07).
        let mut renderer = TuiRenderer::new(80, 24);
        renderer.process(b"line1\n\x1b]0;C:\\WINDOWS");
        renderer.process(b"\\system32\\cmd.exe\x07line2\n");
        let out = renderer.take_pending().unwrap();
        assert!(out.text.contains("line1"));
        assert!(out.text.contains("line2"));
        assert!(!out.text.contains("cmd.exe"));
        assert!(!out.text.contains('\x07'));
    }

    #[test]
    fn test_carry_split_utf8() {
        // 'ä' is two bytes (0xC3 0xA4); split between them.
        let mut renderer = TuiRenderer::new(80, 24);
        renderer.process(&[b'h', b'i', 0xC3]);
        renderer.process(&[0xA4, b'\n']);
        let out = renderer.take_pending().unwrap();
        assert!(out.text.contains("hiä"));
        assert!(!out.text.contains('\u{FFFD}'));
    }

    #[test]
    fn test_carry_split_multibyte_utf8() {
        // 4-byte codepoint (a CJK extension B character) split 1+3.
        let bytes = "𠮷".as_bytes();
        assert_eq!(bytes.len(), 4);
        let mut renderer = TuiRenderer::new(80, 24);
        renderer.process(&bytes[..1]);
        renderer.process(&bytes[1..]);
        renderer.process(b"\n");
        let out = renderer.take_pending().unwrap();
        assert!(out.text.contains("𠮷"));
        assert!(!out.text.contains('\u{FFFD}'));
    }

    #[test]
    fn test_handoff_streaming_then_tui() {
        // Pre-TUI streaming output should be posted as its own streaming
        // message before the first TUI frame is emitted.
        let mut renderer = TuiRenderer::new(20, 5);
        renderer.process(b"about to launch tui\n");
        renderer.process(b"\x1b[?1049h\x1b[2J\x1b[1;1Hframe");
        let first = renderer.take_pending().unwrap();
        assert!(!first.is_edit, "handoff should post, not edit");
        assert!(first.text.contains("about to launch tui"));
        let second = renderer.take_pending().unwrap();
        assert!(second.is_edit, "subsequent TUI frame should edit");
        assert!(second.text.contains("frame"));
    }

    #[test]
    fn test_resize() {
        let mut renderer = TuiRenderer::new(80, 24);
        renderer.resize(120, 40);
        assert_eq!(renderer.cols, 120);
        assert_eq!(renderer.rows, 40);
    }

    #[test]
    fn test_cursor_movement() {
        let mut renderer = TuiRenderer::new(10, 5);
        renderer.tui_mode = true;
        // Move to row 3, col 5 and write 'X'
        renderer.process(b"\x1b[3;5HX");
        assert_eq!(renderer.screen[2][4], 'X');
    }

    #[test]
    fn test_line_wrap() {
        let mut renderer = TuiRenderer::new(5, 3);
        renderer.tui_mode = true;
        renderer.process(b"\x1b[1;1H12345X");
        // 'X' should wrap to next line
        assert_eq!(renderer.screen[1][0], 'X');
    }
}
