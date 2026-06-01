/// Default scrollback line cap. Slack chat.update tops out around 40 KB; at
/// 120 cols × 200 lines that's ~24 KB even with all cells filled, leaving
/// room for the live frame on top. Tunable via `--scrollback`.
pub const DEFAULT_SCROLLBACK_LINES: usize = 200;

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
/// - Rows that scroll off the top of the virtual screen are captured into a
///   bounded scrollback ring and rendered above the live frame, so users can
///   see content that the app pushed past the top.
pub struct TuiRenderer {
    /// Current screen buffer (rows x cols)
    screen: Vec<Vec<char>>,
    /// Terminal dimensions
    cols: usize,
    rows: usize,
    /// Cursor position
    cursor_row: usize,
    cursor_col: usize,
    /// "Pending wrap" / last-column-wrap flag (xterm-compatible behavior).
    /// When the cursor is in the rightmost column and a printable char is
    /// written, we put the char and set this flag *without* advancing
    /// past the right edge or wrapping. Only the NEXT printable char
    /// triggers the actual wrap. CR/LF/cursor-positioning clear it.
    ///
    /// Without this, an exactly-screen-width write (e.g. a 120-char
    /// separator on a 120-col screen) eagerly wraps to the next row and
    /// — if we were at the bottom — scrolls. Modern TUIs (Claude Code)
    /// rely on this not happening: they fill the bottom row exactly, then
    /// expect cursor positioning to reach the same content again.
    pending_wrap: bool,
    /// Saved cursor position from the last DECSC (`\x1b 7`) or SCP (`\x1b[s`).
    /// Restored by DECRC / RCP. Apps drive spinner animations off this:
    /// "save here, write text, [later] restore and overwrite" each frame.
    /// Without tracking it, every restore was a no-op and each frame wrote
    /// wherever the cursor happened to be — frames stacked vertically
    /// instead of overwriting in place.
    saved_cursor: Option<(usize, usize)>,
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
    /// Rows that scrolled off the top of the live screen since the last anchor
    /// reset. Capped at `scrollback_max` lines; oldest evicted first. Rendered
    /// above the live frame in the same Slack message so users can scroll back
    /// to text the app pushed past the top.
    ///
    /// Captures legitimate scrolls (LF past bottom, auto-wrap past bottom,
    /// CSI S "scroll up"). Skips deletions (CSI M, CSI 2J) — those are
    /// intentional content removal, not scrolled-off history.
    scrollback: std::collections::VecDeque<Vec<char>>,
    /// Maximum scrollback lines to retain. 0 disables scrollback entirely
    /// (rows that scroll off are dropped — original behavior).
    scrollback_max: usize,
    /// Whether the TUI screen has changed since the last `take_pending`.
    dirty: bool,
}

impl TuiRenderer {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self::with_scrollback(cols, rows, DEFAULT_SCROLLBACK_LINES)
    }

    /// Build a renderer with an explicit scrollback cap. `0` disables.
    pub fn with_scrollback(cols: u16, rows: u16, scrollback_max: usize) -> Self {
        let cols = cols as usize;
        let rows = rows as usize;
        Self {
            screen: vec![vec![' '; cols]; rows],
            cols,
            rows,
            cursor_row: 0,
            cursor_col: 0,
            pending_wrap: false,
            saved_cursor: None,
            tui_mode: false,
            line_buffer: String::new(),
            pending_handoff: None,
            input_carry: Vec::new(),
            scrollback: std::collections::VecDeque::with_capacity(scrollback_max.min(1024)),
            scrollback_max,
            dirty: false,
        }
    }

    /// Drop all scrollback. Called by the bridge when it re-anchors a Slack
    /// message (start of session, `--clear`, anchor-refresh threshold) so the
    /// next message starts with a clean slate.
    pub fn clear_scrollback(&mut self) {
        self.scrollback.clear();
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
                    Some(&c) => {
                        // Single-char escapes — we drop the introducer so it
                        // doesn't render as a literal. A few are well-known
                        // (=/> for application-keypad mode, M reverse-index,
                        // 7/8 save/restore cursor); anything else gets a
                        // debug log so we can see if a TUI is using something
                        // we should be emulating.
                        let consumed = c;
                        chars.next();
                        match consumed {
                            '7' => {
                                // DECSC — equivalent to CSI s. Spinner
                                // animations rely on this round-tripping.
                                self.saved_cursor = Some((self.cursor_row, self.cursor_col));
                            }
                            '8' => {
                                // DECRC — equivalent to CSI u.
                                if let Some((row, col)) = self.saved_cursor {
                                    self.cursor_row = row.min(self.rows.saturating_sub(1));
                                    self.cursor_col = col.min(self.cols.saturating_sub(1));
                                }
                            }
                            '=' | '>' | 'M' | 'D' | 'E' | 'H' | 'c' => {}
                            _ => tracing::debug!("TUI parser dropped ESC {consumed:?}"),
                        }
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
        // Trace the dispatch with cursor before/after so a captured PTY log
        // replayed under RUST_LOG=bridge_slack=trace is grep-able for the
        // exact moment a row drift starts.
        let before = (self.cursor_row, self.cursor_col);
        // Any explicit cursor movement clears the pending-wrap flag —
        // pending wrap only applies to the *implicit* "next char wraps"
        // semantics; jumping somewhere new resets that.
        self.pending_wrap = false;
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
            'X' => {
                // ECH — Erase Character: blank N cells starting at the cursor,
                // without moving the cursor. Modern TUIs (Claude Code, Ink-
                // based tools) use this heavily during partial redraws — they
                // clear out a region by ECH, then write fresh content. Without
                // handling it, leftover text from earlier frames stays in our
                // buffer and bleeds through into the next render.
                let n = nums.first().copied().unwrap_or(1).max(1);
                let end = (self.cursor_col + n).min(self.cols);
                self.screen[self.cursor_row][self.cursor_col..end].fill(' ');
            }
            'P' => {
                // DCH — Delete Character: drop N chars at the cursor and
                // shift the rest of the line left, padding the tail with spaces.
                let n = nums.first().copied().unwrap_or(1).max(1);
                let row = &mut self.screen[self.cursor_row];
                let row_len = row.len();
                let c = self.cursor_col.min(row_len);
                let n = n.min(row_len - c);
                if n > 0 {
                    row.copy_within(c + n..row_len, c);
                    row[row_len - n..].fill(' ');
                }
            }
            '@' => {
                // ICH — Insert Character: shift the line right by N at the
                // cursor, padding the gap with spaces.
                let n = nums.first().copied().unwrap_or(1).max(1);
                let row = &mut self.screen[self.cursor_row];
                let row_len = row.len();
                if self.cursor_col < row_len {
                    let n = n.min(row_len - self.cursor_col);
                    row.copy_within(self.cursor_col..row_len - n, self.cursor_col + n);
                    row[self.cursor_col..self.cursor_col + n].fill(' ');
                }
            }
            'L' => {
                // IL — Insert Line: insert N blank lines at the cursor row,
                // pushing existing lines down. Lines that fall off the bottom
                // are discarded.
                let n = nums.first().copied().unwrap_or(1).max(1);
                let cols = self.cols;
                let start = self.cursor_row;
                for _ in 0..n.min(self.rows - start) {
                    self.screen.insert(start, vec![' '; cols]);
                    self.screen.pop();
                }
            }
            'M' => {
                // DL — Delete Line: drop N lines at the cursor row, shifting
                // the rest up. Pad the bottom with blank lines.
                let n = nums.first().copied().unwrap_or(1).max(1);
                let cols = self.cols;
                let start = self.cursor_row;
                for _ in 0..n.min(self.rows - start) {
                    self.screen.remove(start);
                    self.screen.push(vec![' '; cols]);
                }
            }
            'G' => {
                // CHA — Cursor Horizontal Absolute: move cursor to col N
                // (1-indexed), keeping the current row.
                let col = nums.first().copied().unwrap_or(1).saturating_sub(1);
                self.cursor_col = col.min(self.cols - 1);
            }
            'd' => {
                // VPA — Vertical Position Absolute: move cursor to row N
                // (1-indexed), keeping the current column.
                let row = nums.first().copied().unwrap_or(1).saturating_sub(1);
                self.cursor_row = row.min(self.rows - 1);
            }
            'S' => {
                // SU — Scroll Up by N lines (drop the top N, append blanks).
                // Each dropped row goes into scrollback.
                let n = nums.first().copied().unwrap_or(1).max(1);
                for _ in 0..n.min(self.rows) {
                    self.scroll_off_top();
                }
            }
            'T' => {
                // SD — Scroll Down by N lines (insert blanks at top, drop bottom).
                let n = nums.first().copied().unwrap_or(1).max(1);
                let cols = self.cols;
                for _ in 0..n.min(self.rows) {
                    self.screen.insert(0, vec![' '; cols]);
                    self.screen.pop();
                }
            }
            'm' => {
                // SGR (colors/attributes) — we intentionally ignore these
                // for text rendering. Slack code blocks don't render inline
                // styling; tracking SGR state would just bloat the buffer.
            }
            'h' | 'l' => {
                // DECSET / DECRST mode set/reset (e.g. ?25h show cursor,
                // ?25l hide, ?2004h bracketed paste). We don't emulate any
                // mode the parser cares about, so silently consume.
            }
            'r' => {
                // DECSTBM — set scrolling region. We always treat the whole
                // screen as the scroll region, which is right for the apps
                // we care about (vim/htop/Claude Code) where scroll regions
                // don't materially change layout in the rendered frame.
            }
            's' => {
                // SCP — Save Cursor Position. See `saved_cursor` field doc
                // for why we track this (spinner animations break otherwise).
                self.saved_cursor = Some((self.cursor_row, self.cursor_col));
            }
            'u' => {
                // RCP — Restore Cursor Position.
                if let Some((row, col)) = self.saved_cursor {
                    self.cursor_row = row.min(self.rows.saturating_sub(1));
                    self.cursor_col = col.min(self.cols.saturating_sub(1));
                }
            }
            'n' => {
                // DSR — Device Status Report query (e.g. ?6n cursor position).
                // We don't have a back-channel to the PTY for replies and
                // most apps degrade gracefully without one.
            }
            't' => {
                // XTWINOPS. Subcode 8 is "resize window to <rows>;<cols>";
                // some TUIs (Claude Code) emit this and then proceed to
                // render assuming the new size, ignoring whatever the PTY
                // told them at startup. If we don't honor it, the app draws
                // outside our virtual-screen bounds and rows collide on the
                // bottom (the "ghost row of mixed-state text" artifact).
                //
                // We only resize our renderer; we deliberately do NOT touch
                // the underlying PTY because the local attach terminal owns
                // that size. Cap to keep a buggy app from asking for 65k
                // rows and making the rendered Slack message useless.
                const MAX_DIM: usize = 500;
                if let Some(&8) = nums.first() {
                    let new_rows = nums.get(1).copied().unwrap_or(self.rows).min(MAX_DIM);
                    let new_cols = nums.get(2).copied().unwrap_or(self.cols).min(MAX_DIM);
                    if new_rows >= 1 && new_cols >= 1 {
                        self.resize(new_cols as u16, new_rows as u16);
                    }
                }
                // Other subcodes (report size, raise/lower, etc.) silently
                // consumed — we have no real window to manipulate and most
                // apps have a back-channel-free degradation.
            }
            'q' => {
                // XTVERSION query (`>0q`) and DECSCUSR cursor-shape (`<n> q`).
                // Neither needs emulation: we have no back-channel for the
                // version reply, and Slack code blocks don't render cursor
                // shapes anyway.
            }
            _ => {
                // Anything else: log at debug so users with RUST_LOG=trace
                // (or debug) can see what we're dropping. If an unhandled
                // CSI is causing visible artifacts, this is the breadcrumb.
                tracing::debug!("TUI parser dropped CSI: \\x1b[{params}{cmd}");
            }
        }
        let after = (self.cursor_row, self.cursor_col);
        if before != after {
            tracing::trace!("CSI \\x1b[{params}{cmd}: cursor {before:?} -> {after:?}");
        }
    }

    /// Drop the top row off the screen, capturing it into scrollback, and
    /// append a fresh blank row at the bottom. Called by the three real-scroll
    /// paths (LF past bottom, auto-wrap past bottom, CSI S "scroll up").
    fn scroll_off_top(&mut self) {
        let dropped = self.screen.remove(0);
        self.screen.push(vec![' '; self.cols]);
        if self.scrollback_max > 0 {
            // Skip rows that are all blanks — they're padding, not content.
            // Also skip the trailing run of blanks on real rows so we don't
            // pad scrollback with right-edge whitespace.
            let last_nonblank = dropped.iter().rposition(|&c| c != ' ');
            if let Some(end) = last_nonblank {
                let trimmed: Vec<char> = dropped[..=end].to_vec();
                if self.scrollback.len() >= self.scrollback_max {
                    self.scrollback.pop_front();
                }
                self.scrollback.push_back(trimmed);
            }
        }
    }

    fn put_char(&mut self, ch: char) {
        match ch {
            '\n' => {
                self.pending_wrap = false;
                self.cursor_row += 1;
                if self.cursor_row >= self.rows {
                    self.scroll_off_top();
                    self.cursor_row = self.rows - 1;
                }
            }
            '\r' => {
                self.pending_wrap = false;
                self.cursor_col = 0;
            }
            '\x08' => {
                // Backspace
                self.pending_wrap = false;
                self.cursor_col = self.cursor_col.saturating_sub(1);
            }
            '\t' => {
                // Tab — advance to next 8-column boundary
                self.pending_wrap = false;
                let next_tab = (self.cursor_col / 8 + 1) * 8;
                self.cursor_col = next_tab.min(self.cols - 1);
            }
            c if !c.is_control() && self.cursor_col < self.cols && self.cursor_row < self.rows => {
                // xterm-style pending-wrap: if the previous printable char
                // already filled the rightmost column, NOW we wrap and
                // place this char on the next line. This is what TUIs
                // expect when filling exactly cols-wide content (e.g. a
                // separator line that exactly equals the screen width).
                if self.pending_wrap {
                    self.pending_wrap = false;
                    self.cursor_col = 0;
                    self.cursor_row += 1;
                    if self.cursor_row >= self.rows {
                        self.scroll_off_top();
                        self.cursor_row = self.rows - 1;
                    }
                }
                self.screen[self.cursor_row][self.cursor_col] = c;
                if self.cursor_col + 1 >= self.cols {
                    // Park at the right edge and defer the wrap until the
                    // *next* printable arrives. CR/LF/positioning clear it.
                    self.pending_wrap = true;
                } else {
                    self.cursor_col += 1;
                }
            }
            _ => {}
        }
    }

    fn render_screen(&self) -> String {
        // Trim trailing all-blank rows: TUI apps often resize themselves
        // larger than they actually use (Claude Code asks for 30 rows but
        // only paints into 25), and Slack's monospace block wraps anything
        // we emit, so empty rows just inflate the message.
        let last_nonblank = self
            .screen
            .iter()
            .rposition(|row| row.iter().any(|&c| c != ' '))
            .map(|i| i + 1)
            .unwrap_or(0);

        let mut output = String::from("```\n");
        // Scrollback first — rows that have scrolled off the top of the
        // live screen since the last anchor reset. Already trimmed of
        // trailing whitespace when captured.
        for row in &self.scrollback {
            for &c in row {
                output.push(c);
            }
            output.push('\n');
        }
        for row in &self.screen[..last_nonblank] {
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

    fn row_str(r: &[char]) -> String {
        r.iter().collect::<String>().trim_end().to_string()
    }

    #[test]
    fn test_csi_ech_erases_in_place() {
        // ECH (\x1b[<n>X) clears N cells without moving the cursor — used by
        // partial-redraw TUIs (Claude Code etc.) to scrub a region before
        // writing fresh content. Without this we'd carry "old" text forward.
        let mut renderer = TuiRenderer::new(20, 1);
        renderer.tui_mode = true;
        renderer.process(b"\x1b[1;1Hold text here");
        // Move to col 5 ("text"), erase 4 cells.
        renderer.process(b"\x1b[1;5H\x1b[4X");
        let line = row_str(&renderer.screen[0]);
        assert_eq!(line, "old      here");
    }

    #[test]
    fn test_csi_dch_deletes_chars() {
        // Wider than the content so put_char's auto-wrap doesn't trip the
        // single row into scrolling.
        let mut renderer = TuiRenderer::new(20, 2);
        renderer.tui_mode = true;
        renderer.process(b"\x1b[1;1Habcdefghij");
        renderer.process(b"\x1b[1;3H\x1b[2P"); // at col 3, delete 2
        assert_eq!(row_str(&renderer.screen[0]), "abefghij");
    }

    #[test]
    fn test_csi_ich_inserts_chars() {
        // Width 10 row of content lives on a 12-wide / 2-row screen.
        let mut renderer = TuiRenderer::new(12, 2);
        renderer.tui_mode = true;
        renderer.process(b"\x1b[1;1Habcdefghij");
        renderer.process(b"\x1b[1;3H\x1b[2@"); // at col 3, insert 2 spaces
        // "ab" + "  " + "cdefghij" (no fall-off since width is 12, but the
        // tail beyond the original 10 cols stays blank)
        let line: String = renderer.screen[0].iter().collect();
        // Trim trailing blanks for the assertion.
        assert_eq!(line.trim_end(), "ab  cdefghij");
    }

    #[test]
    fn test_csi_il_inserts_lines() {
        // Wider than content so 5-char writes don't auto-wrap.
        let mut renderer = TuiRenderer::new(10, 4);
        renderer.tui_mode = true;
        renderer.process(b"\x1b[1;1Haaaaa");
        renderer.process(b"\x1b[2;1Hbbbbb");
        renderer.process(b"\x1b[3;1Hccccc");
        // At row 2, insert 1 blank line.
        renderer.process(b"\x1b[2;1H\x1b[1L");
        assert_eq!(row_str(&renderer.screen[0]), "aaaaa");
        assert_eq!(row_str(&renderer.screen[1]), ""); // blank
        assert_eq!(row_str(&renderer.screen[2]), "bbbbb");
        assert_eq!(row_str(&renderer.screen[3]), "ccccc");
    }

    #[test]
    fn test_csi_dl_deletes_lines() {
        let mut renderer = TuiRenderer::new(10, 4);
        renderer.tui_mode = true;
        renderer.process(b"\x1b[1;1Haaaaa");
        renderer.process(b"\x1b[2;1Hbbbbb");
        renderer.process(b"\x1b[3;1Hccccc");
        renderer.process(b"\x1b[4;1Hddddd");
        // At row 2, delete 1 line.
        renderer.process(b"\x1b[2;1H\x1b[1M");
        assert_eq!(row_str(&renderer.screen[0]), "aaaaa");
        assert_eq!(row_str(&renderer.screen[1]), "ccccc");
        assert_eq!(row_str(&renderer.screen[2]), "ddddd");
        assert_eq!(row_str(&renderer.screen[3]), ""); // blank
    }

    #[test]
    fn test_csi_cha_horizontal_absolute() {
        let mut renderer = TuiRenderer::new(20, 1);
        renderer.tui_mode = true;
        renderer.process(b"\x1b[1;1HABC");
        renderer.process(b"\x1b[10GZ"); // jump to col 10, write Z
        assert_eq!(renderer.screen[0][9], 'Z');
        // Earlier text untouched.
        assert_eq!(renderer.screen[0][0], 'A');
    }

    #[test]
    fn test_csi_vpa_vertical_absolute() {
        let mut renderer = TuiRenderer::new(5, 4);
        renderer.tui_mode = true;
        renderer.process(b"\x1b[1;1HA");
        renderer.process(b"\x1b[3dB"); // jump to row 3 (preserving col... col is now 1 after the 'A')
        // Cursor advanced past A so col=1; row jumps to 2 (0-indexed)
        assert_eq!(renderer.screen[2][1], 'B');
    }

    #[test]
    fn test_save_restore_cursor_csi() {
        // SCP/RCP must round-trip — apps animate spinners by saving cursor,
        // writing a frame, restoring, overwriting on the next frame. Without
        // this, frames stack vertically (the original "Flambéing… /
        // Cogitated…" double-line bug).
        let mut renderer = TuiRenderer::new(20, 3);
        renderer.tui_mode = true;
        renderer.process(b"\x1b[2;5H"); // row 2, col 5
        renderer.process(b"\x1b[s"); // save
        renderer.process(b"\x1b[1;1Helsewhere"); // move + write
        renderer.process(b"\x1b[u"); // restore
        renderer.process(b"X");
        // X should land at row 1 (0-idx), col 4.
        assert_eq!(renderer.screen[1][4], 'X');
    }

    #[test]
    fn test_save_restore_cursor_decsc_decrc() {
        // \x1b 7 / \x1b 8 — same effect as SCP/RCP, more common in modern apps.
        let mut renderer = TuiRenderer::new(20, 3);
        renderer.tui_mode = true;
        renderer.process(b"\x1b[3;10H"); // row 3, col 10
        renderer.process(b"\x1b7"); // DECSC
        renderer.process(b"\x1b[1;1Hsomewhere else");
        renderer.process(b"\x1b8"); // DECRC
        renderer.process(b"Y");
        assert_eq!(renderer.screen[2][9], 'Y');
    }

    #[test]
    fn test_save_restore_no_op_when_unsaved() {
        // RCP before SCP: should leave cursor where it was rather than panic
        // or jump somewhere weird.
        let mut renderer = TuiRenderer::new(20, 3);
        renderer.tui_mode = true;
        renderer.process(b"\x1b[2;5H"); // row 2 col 5
        renderer.process(b"\x1b[u"); // restore with no save
        renderer.process(b"Z");
        // Cursor stayed at row 2 col 5 (we wrote no save), so Z lands there.
        assert_eq!(renderer.screen[1][4], 'Z');
    }

    #[test]
    fn test_scrollback_captures_lf_scroll() {
        // Wider than content + room for the trailing write so we don't
        // accidentally trigger an extra auto-wrap scroll.
        let mut renderer = TuiRenderer::with_scrollback(20, 2, 50);
        renderer.tui_mode = true;
        renderer.process(b"\x1b[1;1Hfirst");
        renderer.process(b"\x1b[2;1Hsecond");
        // Position to end of "second" then LF past bottom — row 0 ("first")
        // scrolls off into scrollback.
        renderer.process(b"\x1b[2;7H\n");
        renderer.process(b"third");
        assert_eq!(renderer.scrollback.len(), 1);
        let s: String = renderer.scrollback[0].iter().collect();
        assert_eq!(s, "first");
    }

    #[test]
    fn test_scrollback_renders_above_live_frame() {
        let mut renderer = TuiRenderer::with_scrollback(10, 2, 50);
        renderer.tui_mode = true;
        renderer.process(b"\x1b[1;1Hold");
        renderer.process(b"\x1b[2;1Hkeep");
        renderer.process(b"\n"); // 'old' scrolls off
        renderer.process(b"new");
        let rendered = renderer.take_pending().unwrap();
        // Both old (scrollback) and new content present, in chronological order.
        let old_pos = rendered.text.find("old").expect("scrollback missing");
        let keep_pos = rendered.text.find("keep").expect("live row missing");
        let new_pos = rendered.text.find("new").expect("live row missing");
        assert!(old_pos < keep_pos);
        assert!(keep_pos < new_pos);
    }

    #[test]
    fn test_scrollback_capped_at_max() {
        let mut renderer = TuiRenderer::with_scrollback(10, 2, 3);
        renderer.tui_mode = true;
        // Push 10 rows through (CRLF line-discipline-style so col resets).
        for i in 0..10 {
            renderer.process(format!("row{i}\r\n").as_bytes());
        }
        assert_eq!(renderer.scrollback.len(), 3);
        // Last entry should be one of the most recent rows.
        let last: String = renderer.scrollback.back().unwrap().iter().collect();
        assert!(last.starts_with("row"), "got: {last:?}");
    }

    #[test]
    fn test_scrollback_disabled_when_max_zero() {
        let mut renderer = TuiRenderer::with_scrollback(10, 2, 0);
        renderer.tui_mode = true;
        for i in 0..5 {
            renderer.process(format!("row{i}\r\n").as_bytes());
        }
        assert!(renderer.scrollback.is_empty());
    }

    #[test]
    fn test_scrollback_clear() {
        let mut renderer = TuiRenderer::with_scrollback(10, 2, 50);
        renderer.tui_mode = true;
        for i in 0..3 {
            renderer.process(format!("r{i}\r\n").as_bytes());
        }
        assert!(!renderer.scrollback.is_empty());
        renderer.clear_scrollback();
        assert!(renderer.scrollback.is_empty());
    }

    #[test]
    fn test_scrollback_skips_blank_rows() {
        // Scrolling off a row that's all blanks shouldn't pollute scrollback
        // — those are padding, not user content.
        let mut renderer = TuiRenderer::with_scrollback(10, 2, 50);
        renderer.tui_mode = true;
        // Start with two blank rows. \n past bottom scrolls a blank off.
        renderer.process(b"\n\n");
        assert!(renderer.scrollback.is_empty());
    }

    #[test]
    fn test_csi_position_clamps_out_of_bounds() {
        // \x1b[<r>;<c>H with values past the screen size must clamp,
        // not panic or render outside the buffer.
        let mut renderer = TuiRenderer::new(20, 5);
        renderer.tui_mode = true;
        // Way past bottom-right.
        renderer.process(b"\x1b[999;999HX");
        // Char must land inside the screen.
        let bottom_right_row = renderer.rows - 1;
        let bottom_right_col = renderer.cols - 1;
        assert_eq!(renderer.screen[bottom_right_row][bottom_right_col], 'X');
    }

    #[test]
    fn test_csi_position_clamps_zero() {
        // \x1b[0;0H is technically out-of-spec (params are 1-indexed) but
        // some apps emit it. We must not underflow.
        let mut renderer = TuiRenderer::new(10, 3);
        renderer.tui_mode = true;
        renderer.process(b"\x1b[0;0HZ");
        assert_eq!(renderer.screen[0][0], 'Z');
    }

    #[test]
    fn test_pending_wrap_does_not_scroll_at_bottom() {
        // Regression: writing exactly cols-wide content on the bottom row
        // used to wrap eagerly to a non-existent next row, scrolling the
        // screen up. xterm-style pending-wrap defers the wrap until the
        // *next* printable, so apps that fill the bottom row exactly
        // (Claude Code's separator lines) don't trigger spurious scrolls.
        let mut renderer = TuiRenderer::new(5, 3);
        renderer.tui_mode = true;
        // Pin "anch" on row 0 so we can detect a scroll.
        renderer.process(b"\x1b[1;1Hanch");
        // Move to bottom row, write exactly 5 chars (one full row).
        renderer.process(b"\x1b[3;1Habcde");
        // Anchor must still be on row 0 — no scroll yet.
        let line0: String = renderer.screen[0].iter().collect();
        assert!(
            line0.starts_with("anch"),
            "scroll happened too early: row 0 = {line0:?}"
        );
        // The 6th printable arrives — NOW we wrap. Since we're on the
        // bottom row, the wrap scrolls.
        renderer.process(b"X");
        let line0: String = renderer.screen[0].iter().collect();
        assert!(
            !line0.starts_with("anch"),
            "expected scroll after the deferred wrap, but anchor still on row 0"
        );
    }

    #[test]
    fn test_pending_wrap_cleared_by_cursor_position() {
        // After a deferred-wrap fill, an explicit cursor-position must clear
        // the pending flag — otherwise the next printable would jump to the
        // wrong row.
        let mut renderer = TuiRenderer::new(5, 3);
        renderer.tui_mode = true;
        renderer.process(b"\x1b[1;1Habcde"); // row 0 full, pending wrap
        renderer.process(b"\x1b[2;1HX"); // jump to row 1 col 0, write X
        // X should land at row 1 col 0, NOT row 1 col 0 after a wrap.
        assert_eq!(renderer.screen[1][0], 'X');
        // Row 0 still has "abcde".
        let line0: String = renderer.screen[0].iter().collect();
        assert_eq!(line0, "abcde");
    }

    #[test]
    fn test_pending_wrap_cleared_by_cr() {
        // CR also clears pending wrap.
        let mut renderer = TuiRenderer::new(5, 2);
        renderer.tui_mode = true;
        renderer.process(b"\x1b[1;1Habcde"); // row 0 full
        renderer.process(b"\rX"); // CR + X — should overwrite col 0 of row 0
        assert_eq!(renderer.screen[0][0], 'X');
        assert_eq!(renderer.screen[0][1], 'b');
    }

    #[test]
    fn test_csi_su_sd_scroll() {
        // Wider than the row content so put_char doesn't auto-wrap and
        // muddy the scroll math.
        let mut renderer = TuiRenderer::new(5, 3);
        renderer.tui_mode = true;
        renderer.process(b"\x1b[1;1H111\x1b[2;1H222\x1b[3;1H333");
        renderer.process(b"\x1b[1S"); // scroll up 1
        assert_eq!(row_str(&renderer.screen[0]), "222");
        assert_eq!(row_str(&renderer.screen[1]), "333");
        assert_eq!(row_str(&renderer.screen[2]), "");

        renderer.process(b"\x1b[1T"); // scroll down 1
        assert_eq!(row_str(&renderer.screen[0]), "");
        assert_eq!(row_str(&renderer.screen[1]), "222");
        assert_eq!(row_str(&renderer.screen[2]), "333");
    }
}
