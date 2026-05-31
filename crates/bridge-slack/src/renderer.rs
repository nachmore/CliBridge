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
            dirty: false,
        }
    }

    /// Append raw terminal output bytes to the renderer's buffer. Cheap and
    /// allocation-light; the bridge calls `take_pending()` on a tick to actually
    /// produce a message.
    pub fn process(&mut self, data: &[u8]) {
        let text = String::from_utf8_lossy(data);

        if !self.tui_mode && Self::is_tui_output(&text) {
            self.tui_mode = true;
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
        // Only enable TUI mode for the alternate-screen-buffer escape (DECSET
        // ?1049 or its older sibling ?47). Apps that take over the screen
        // (vim, htop, less, tmux) emit this; cmd.exe / PowerShell prompts use
        // ?25l and \x1b[H during normal echo on Windows ConPTY, so those alone
        // would mis-classify regular output as a TUI frame.
        text.contains("\x1b[?1049h") || text.contains("\x1b[?47h")
    }

    fn process_tui(&mut self, text: &str) {
        let mut chars = text.chars().peekable();

        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                // Parse ANSI escape sequence
                if chars.peek() == Some(&'[') {
                    chars.next(); // consume '['
                    let mut params = String::new();
                    while let Some(&c) = chars.peek() {
                        if c.is_ascii_digit() || c == ';' || c == '?' {
                            params.push(c);
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
                // Skip other escape sequences (OSC, etc.)
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
        let nums: Vec<usize> = params
            .split(';')
            .filter(|s| !s.is_empty() && !s.starts_with('?'))
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

/// Strip ANSI escape sequences from text.
fn strip_ansi(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            // Skip the escape sequence
            if chars.peek() == Some(&'[') {
                chars.next();
                // Skip until we hit a letter
                while let Some(&c) = chars.peek() {
                    chars.next();
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            result.push(ch);
        }
    }

    result
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
        // Only alternate-screen escapes flip TUI mode.
        assert!(TuiRenderer::is_tui_output("\x1b[?1049h"));
        assert!(TuiRenderer::is_tui_output("\x1b[?47h"));
        // Cmd.exe / PowerShell echo includes these but is NOT a TUI app.
        assert!(!TuiRenderer::is_tui_output("\x1b[H"));
        assert!(!TuiRenderer::is_tui_output("\x1b[?25l"));
        assert!(!TuiRenderer::is_tui_output("\x1b[2J"));
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
