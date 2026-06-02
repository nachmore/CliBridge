//! Asynchronous transcript dispatch.
//!
//! The bridge's session loop must never block on a messaging platform's
//! network calls — if it does, PTY output stops being forwarded to the local
//! terminal and typed input stops reaching the shell (both visible as ~1s
//! stalls, since platforms like Slack rate-limit posting to ~1 msg/sec).
//!
//! This module moves all posting onto a dedicated task. The session loop
//! hands the dispatcher *snapshots* (scrolled-off rows + the current live
//! frame) over a bounded channel using non-blocking sends; if the dispatcher
//! is behind, the session simply skips the hand-off and the renderer's own
//! ring buffer (bounded, oldest-evicting) absorbs the backlog. The dispatcher
//! drains that backlog at whatever pace the platform's rate limiter allows.
//!
//! The design is platform-agnostic so a future listener (Teams, Discord, …)
//! reuses it: the dispatcher talks to a [`TranscriptSink`] (any
//! [`MessagingClient`] qualifies via a blanket impl) and formats scroll-buffer
//! messages through a [`TranscriptFormat`]. The "two-level scroll buffer"
//! policy (an actively-extended buffer message above a series of sealed
//! history messages) lives here, independent of which platform renders it.

use std::collections::VecDeque;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, error, warn};

use crate::error::BridgeError;
use crate::messaging::MessagingClient;

/// Channel depth between the session loop and the dispatcher task. Each slot
/// holds one per-tick snapshot. This is intentionally generous: it's a second
/// line of defense behind the renderer's own scroll-buffer ring, so a long
/// burst of output (an LLM dumping thousands of lines, `find /`, etc.) is
/// absorbed without the session loop ever having to block. When it *does*
/// fill, the session skips the hand-off rather than stalling, and the
/// renderer ring (which evicts oldest) bounds total memory.
const DISPATCH_CHANNEL_DEPTH: usize = 4096;

/// A single live-frame snapshot handed to the dispatcher.
///
/// `is_edit` distinguishes the two output modes the renderer produces:
/// - `true` — a TUI "live screen" frame. Successive frames are idempotent
///   snapshots of the same logical message, so the dispatcher coalesces them
///   (only the newest matters) and posts via *edit*.
/// - `false` — a distinct streaming chunk (plain scrolling output, or the
///   pre-TUI hand-off). Each one is its own message and must be posted; they
///   are never coalesced or dropped.
#[derive(Debug, Clone)]
pub struct LiveFrame {
    pub text: String,
    pub is_edit: bool,
}

/// Messages the session loop sends to the dispatcher task.
enum DispatchMsg {
    /// A per-tick snapshot: rows that scrolled off the screen since the last
    /// snapshot (oldest-first, already sanitized for the platform) plus the
    /// current live frame, if any.
    Frame {
        rows: Vec<Vec<char>>,
        live: Option<LiveFrame>,
    },
    /// Re-anchor: seal the active scroll buffer as history and force the next
    /// live frame to post as a fresh message. Sent on `--clear` and the
    /// periodic anchor-refresh.
    Anchor,
    /// Post a standalone message (a command reply or lifecycle banner) into
    /// the transcript, ordered after whatever is already queued.
    Say(String),
}

/// The posting half of a messaging platform, as the dispatcher needs it:
/// create a message (returning its id) and edit one in place. Any
/// [`MessagingClient`] is a `TranscriptSink` via the blanket impl below, so
/// new platforms get async dispatch for free once they implement the client
/// trait.
#[async_trait]
pub trait TranscriptSink: Send + Sync + 'static {
    async fn post(&self, channel: &str, text: &str) -> Result<String, BridgeError>;
    async fn edit(&self, channel: &str, id: &str, text: &str) -> Result<(), BridgeError>;
}

#[async_trait]
impl<T: MessagingClient + ?Sized + 'static> TranscriptSink for T {
    async fn post(&self, channel: &str, text: &str) -> Result<String, BridgeError> {
        self.send_message(channel, text).await
    }
    async fn edit(&self, channel: &str, id: &str, text: &str) -> Result<(), BridgeError> {
        self.edit_message(channel, id, text).await
    }
}

/// Platform-specific formatting for scroll-buffer messages. The live frame
/// arrives already formatted by the renderer, so the only thing the
/// dispatcher formats itself is the scrollback: an actively-extended buffer
/// message and the sealed "history" messages it rolls over into.
///
/// All sizes are in whatever unit the platform's length limit is expressed in
/// — for Slack that's UTF-16 code units, not bytes or `char`s — so the
/// dispatcher measures everything through [`TranscriptFormat::measure`].
pub trait TranscriptFormat: Send + Sync + 'static {
    /// Maximum size of a single message body, in the platform's length unit.
    fn size_limit(&self) -> usize;

    /// Measure a string in the platform's length unit.
    fn measure(&self, s: &str) -> usize;

    /// Render a fresh active scroll-buffer body around the given rows text
    /// (which already has one trailing newline per row).
    fn scroll_body(&self, rows_text: &str) -> String;

    /// Transform an active scroll-buffer body into its sealed "history" form
    /// (same inner rows, relabeled). Called once when an active buffer fills
    /// up and is rolled over.
    fn lock_body(&self, active_body: &str) -> String;

    /// Splice additional rows into an existing active scroll-buffer body,
    /// before its trailing fence/footer.
    fn extend_body(&self, existing: &str, new_rows_text: &str) -> String;

    /// Framing overhead of an empty active scroll-buffer body (header + empty
    /// fence). Used to compute how many rows fit in a fresh message. Defaults
    /// to measuring `scroll_body("")`.
    fn fresh_overhead(&self) -> usize {
        self.measure(&self.scroll_body(""))
    }

    /// Split a fully-formatted message body into one or more bodies, each
    /// within [`size_limit`](Self::size_limit) and individually well-formed
    /// (e.g. with code fences closed). A body that already fits returns
    /// `vec![body]`.
    ///
    /// This is what prevents over-long live frames / streaming chunks from
    /// being posted as a single message that the platform truncates or
    /// splits across bubbles — the bug where a fenced block lost its closing
    /// fence mid-stream. The default returns the body unchanged, so platforms
    /// without a hard ceiling need not implement it; any platform with a
    /// `size_limit` should.
    fn paginate(&self, body: &str) -> Vec<String> {
        vec![body.to_string()]
    }
}

/// Handle the session loop keeps. Sends are non-blocking: if the dispatcher is
/// behind, frame/say hand-offs are dropped (the renderer ring retains the
/// underlying rows) rather than stalling the hot path.
pub struct DispatchHandle {
    tx: mpsc::Sender<DispatchMsg>,
    task: JoinHandle<()>,
}

impl DispatchHandle {
    /// Whether the dispatcher has room for another snapshot right now. The
    /// session loop checks this *before* draining the renderer's scroll-buffer
    /// ring, so that on a full queue the rows stay in the ring (bounded,
    /// oldest-evicting) instead of being pulled out and dropped.
    pub fn has_capacity(&self) -> bool {
        self.tx.capacity() > 0
    }

    /// Offer a per-tick snapshot. Returns `true` if it was accepted, `false`
    /// if the dispatcher's queue is full — in which case the caller should
    /// leave the rows in the renderer ring and try again next tick.
    pub fn try_send_frame(&self, rows: Vec<Vec<char>>, live: Option<LiveFrame>) -> bool {
        // Skip a completely empty snapshot — nothing to do, and it would
        // burn a channel slot.
        if rows.is_empty() && live.is_none() {
            return true;
        }
        self.tx.try_send(DispatchMsg::Frame { rows, live }).is_ok()
    }

    /// Re-anchor the transcript (seal active scroll buffer, next live frame
    /// posts fresh). Best-effort.
    pub fn anchor(&self) {
        if self.tx.try_send(DispatchMsg::Anchor).is_err() {
            warn!("dispatch queue full; dropping anchor request");
        }
    }

    /// Post a standalone message (command reply / banner). Best-effort: under
    /// a saturating output burst this may be dropped rather than block the
    /// session loop. Render output and scrollback are never sacrificed for a
    /// reply line.
    pub fn say(&self, text: impl Into<String>) {
        if self.tx.try_send(DispatchMsg::Say(text.into())).is_err() {
            warn!("dispatch queue full; dropping message");
        }
    }

    /// Close the channel and wait (up to the caller's own timeout) for the
    /// dispatcher to flush whatever it can before the process tears down.
    pub async fn shutdown(self) {
        drop(self.tx);
        if let Err(e) = self.task.await {
            debug!("dispatch task join error on shutdown: {e}");
        }
    }
}

/// One open scroll-buffer message the dispatcher keeps extending in place.
/// `body` is the full current text (header + fence + rows) so size checks are
/// unambiguous before committing to an extend-vs-roll-over decision.
struct ActiveScrollBuffer {
    message_id: String,
    body: String,
}

/// A unit of ordered work in the dispatcher's queue. Live frames are *not*
/// here — they're coalesced separately and always rendered last — but
/// everything that must preserve arrival order relative to scrollback is.
enum Work {
    /// A row that scrolled off the screen. Consecutive `Row`s are drained
    /// together into size-bounded scroll-buffer batches.
    Row(Vec<char>),
    /// A standalone message (command reply / banner) posted as its own
    /// message in the flow.
    Stream(String),
    /// Seal the active scroll buffer as history and force the next live frame
    /// to post fresh. Any rows queued before this are flushed first, so they
    /// land in history above the new live message.
    Anchor,
}

/// The dispatcher task's state. Owns all transcript-posting bookkeeping that
/// used to live inline in the session loop.
struct Dispatcher {
    channel: String,
    sink: std::sync::Arc<dyn TranscriptSink>,
    fmt: Box<dyn TranscriptFormat>,
    rx: mpsc::Receiver<DispatchMsg>,

    /// Ordered work queue: scrolled-off rows, standalone messages, and anchor
    /// requests, in arrival order. Drained one platform-call's worth at a time.
    queue: VecDeque<Work>,
    /// Newest coalesced live-frame text (is_edit=true) we want shown. Always
    /// rendered after the ordered queue is empty.
    latest_live: Option<String>,

    /// The current live message we edit in place (None ⇒ next live posts fresh).
    current_message_id: Option<String>,
    /// The active scroll-buffer message being extended.
    active: Option<ActiveScrollBuffer>,
    /// Last live body actually posted, to skip no-op edits.
    last_live_body: Option<String>,
}

/// Spawn the dispatcher task and return a handle. The session loop feeds the
/// handle; the task posts to `sink` formatted via `fmt`.
pub fn spawn<S>(
    channel: String,
    sink: std::sync::Arc<S>,
    fmt: Box<dyn TranscriptFormat>,
) -> DispatchHandle
where
    S: TranscriptSink,
{
    let (tx, rx) = mpsc::channel(DISPATCH_CHANNEL_DEPTH);
    let dispatcher = Dispatcher {
        channel,
        sink,
        fmt,
        rx,
        queue: VecDeque::new(),
        latest_live: None,
        current_message_id: None,
        active: None,
        last_live_body: None,
    };
    let task = tokio::spawn(dispatcher.run());
    DispatchHandle { tx, task }
}

impl Dispatcher {
    async fn run(mut self) {
        loop {
            if self.has_pending_work() {
                // Opportunistically absorb everything queued so we can
                // coalesce live frames and keep ordering correct, without
                // blocking — then do exactly one rate-limited post.
                let mut closed = false;
                loop {
                    match self.rx.try_recv() {
                        Ok(msg) => self.ingest(msg),
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            closed = true;
                            break;
                        }
                    }
                }
                self.post_one_unit().await;
                if closed && !self.has_pending_work() {
                    break;
                }
            } else {
                match self.rx.recv().await {
                    Some(msg) => self.ingest(msg),
                    // Channel closed and nothing left to flush.
                    None => break,
                }
            }
        }
    }

    fn ingest(&mut self, msg: DispatchMsg) {
        match msg {
            DispatchMsg::Frame { rows, live } => {
                self.queue.extend(rows.into_iter().map(Work::Row));
                if let Some(frame) = live {
                    if frame.is_edit {
                        // Coalesce: only the newest live screen matters.
                        self.latest_live = Some(frame.text);
                    } else {
                        // Distinct chunk: must post in order, after any
                        // scrollback queued ahead of it.
                        self.queue.push_back(Work::Stream(frame.text));
                    }
                }
            }
            DispatchMsg::Anchor => self.queue.push_back(Work::Anchor),
            DispatchMsg::Say(text) => self.queue.push_back(Work::Stream(text)),
        }
    }

    fn has_pending_work(&self) -> bool {
        !self.queue.is_empty()
            || self
                .latest_live
                .as_deref()
                .is_some_and(|t| self.last_live_body.as_deref() != Some(t))
    }

    /// Do exactly one rate-limited platform call. The ordered queue is
    /// processed first (scrollback batches, standalone messages, anchors, in
    /// arrival order); only once it's empty do we render the coalesced live
    /// frame.
    async fn post_one_unit(&mut self) {
        match self.queue.front() {
            Some(Work::Row(_)) => {
                self.flush_one_scroll_batch().await;
            }
            Some(Work::Stream(_)) => {
                let Some(Work::Stream(text)) = self.queue.pop_front() else {
                    unreachable!()
                };
                // Size-bound the body. An over-long chunk becomes multiple
                // pages, each well-formed (fences closed). Post the first page
                // now and requeue the rest at the front so they post next, in
                // order, one rate-limited call apiece.
                let mut pages = self.fmt.paginate(&text);
                if pages.is_empty() {
                    return;
                }
                let first = pages.remove(0);
                for page in pages.into_iter().rev() {
                    self.queue.push_front(Work::Stream(page));
                }
                if let Err(e) = self.sink.post(&self.channel, &first).await {
                    error!("failed to post message: {e}");
                }
            }
            Some(Work::Anchor) => {
                self.queue.pop_front();
                // Seal the active scroll buffer (it sits above the soon-to-be
                // fresh live message and must not be edited further) and force
                // the next live frame to post as a brand-new message.
                if let Some(active) = self.active.take() {
                    self.lock_active(active).await;
                }
                self.current_message_id = None;
                self.last_live_body = None;
            }
            None => {
                if let Some(text) = self.latest_live.clone()
                    && self.last_live_body.as_deref() != Some(text.as_str())
                {
                    self.post_or_edit_live(&text).await;
                }
            }
        }
    }

    async fn post_or_edit_live(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        // The live frame is a single message we edit in place, so it must fit
        // in one body. If the current screen is too dense to fit (a full
        // terminal of long lines can exceed the limit once headers/fences are
        // added), show the *tail* — the bottom of the screen, matching a
        // terminal viewport and where the action usually is. We dedupe on the
        // original `text` upstream, so set last_live_body to that, not the
        // possibly-trimmed page.
        let pages = self.fmt.paginate(text);
        let body = pages.last().map(String::as_str).unwrap_or(text);

        if let Some(id) = self.current_message_id.clone() {
            match self.sink.edit(&self.channel, &id, body).await {
                Ok(()) => {
                    self.last_live_body = Some(text.to_string());
                    return;
                }
                Err(e) => {
                    error!("failed to edit live message, posting fresh: {e}");
                    self.current_message_id = None;
                }
            }
        }
        match self.sink.post(&self.channel, body).await {
            Ok(ts) => {
                self.current_message_id = Some(ts);
                self.last_live_body = Some(text.to_string());
            }
            Err(e) => error!("failed to send live message: {e}"),
        }
    }

    async fn lock_active(&mut self, active: ActiveScrollBuffer) {
        let locked = self.fmt.lock_body(&active.body);
        debug!("locking scroll buffer id={} as history", active.message_id);
        if let Err(e) = self
            .sink
            .edit(&self.channel, &active.message_id, &locked)
            .await
        {
            error!("failed to lock scroll buffer as history: {e}");
        }
    }

    /// Borrow the next queued row, if the front of the queue is one.
    fn peek_front_row(&self) -> Option<&[char]> {
        match self.queue.front() {
            Some(Work::Row(r)) => Some(r.as_slice()),
            _ => None,
        }
    }

    /// Pop the next queued row, if the front of the queue is one.
    fn pop_front_row(&mut self) -> Option<Vec<char>> {
        if matches!(self.queue.front(), Some(Work::Row(_))) {
            match self.queue.pop_front() {
                Some(Work::Row(r)) => Some(r),
                _ => unreachable!(),
            }
        } else {
            None
        }
    }

    /// Drain as many leading rows as fit into one message and post/extend the
    /// active scroll buffer with them. Bounds work to a single platform call
    /// (or a single lock-edit when rolling over a full buffer). Never drops a
    /// row: anything that doesn't fit stays queued for the next call. Stops at
    /// the first non-`Row` work item so ordering with anchors/messages holds.
    async fn flush_one_scroll_batch(&mut self) {
        if self.peek_front_row().is_none() {
            return;
        }

        let limit = self.fmt.size_limit();
        let fresh_overhead = self.fmt.fresh_overhead();

        let available = match &self.active {
            Some(a) => limit.saturating_sub(self.fmt.measure(&a.body)),
            None => limit.saturating_sub(fresh_overhead),
        };

        // If the active can't fit even the next row, seal it and roll over.
        let next_row_size = self.peek_front_row().map(|r| self.row_size(r)).unwrap_or(0);
        if next_row_size > available
            && let Some(active) = self.active.take()
        {
            debug!(
                "active scroll buffer id={} can't fit next row ({} > {}); locking as history",
                active.message_id, next_row_size, available
            );
            self.lock_active(active).await;
            return;
        }

        let budget = if self.active.is_some() {
            available
        } else {
            limit.saturating_sub(fresh_overhead)
        };

        let batch = self.drain_rows_into_budget(budget);
        if batch.is_empty() {
            // The next row is wider than even a fresh message can hold. Hard-
            // split it at a length-unit boundary so we make forward progress;
            // push the remainder back to resume next call.
            if let Some(row) = self.pop_front_row() {
                let mut line: String = row.into_iter().collect();
                line.push('\n');
                let split = self.boundary_at(&line, budget);
                let head = line[..split].to_string();
                let tail: Vec<char> = line[split..].trim_end_matches('\n').chars().collect();
                if !tail.is_empty() {
                    self.queue.push_front(Work::Row(tail));
                }
                error!(
                    "scroll buffer row wider than per-message budget; hard-split at {} units",
                    self.fmt.measure(&head)
                );
                self.ingest_scroll_batch(&head).await;
            }
            return;
        }
        self.ingest_scroll_batch(&batch).await;
    }

    /// Append a ready-to-fit batch to the active scroll buffer, opening a
    /// fresh active (or demoting the current live message into one) as needed,
    /// and rolling over to a new active when the existing one would overflow.
    async fn ingest_scroll_batch(&mut self, batch: &str) {
        // Case A: no active yet — reuse the live message (demote it) or post
        // a fresh active.
        if self.active.is_none() {
            let body = self.fmt.scroll_body(batch);
            if let Some(id) = self.current_message_id.take() {
                match self.sink.edit(&self.channel, &id, &body).await {
                    Ok(()) => {
                        debug!("demoted live message id={id} into active scroll buffer");
                        // The demoted live message is gone; force a fresh post
                        // next live frame.
                        self.last_live_body = None;
                        self.active = Some(ActiveScrollBuffer {
                            message_id: id,
                            body,
                        });
                        return;
                    }
                    Err(e) => {
                        error!(
                            "failed to demote live message to scroll buffer, posting fresh: {e}"
                        );
                    }
                }
            }
            match self.sink.post(&self.channel, &body).await {
                Ok(ts) => {
                    debug!("opened new active scroll buffer id={ts}");
                    self.active = Some(ActiveScrollBuffer {
                        message_id: ts,
                        body,
                    });
                }
                Err(e) => error!("failed to post new active scroll buffer: {e}"),
            }
            return;
        }

        // Case B: extend the existing active, or roll over if it would
        // overflow. Compute against a snapshot of the active's fields so we
        // don't hold a mutable borrow of `self.active` across the `&self`
        // format/sink calls.
        let (active_id, active_body) = {
            let active = self.active.as_ref().unwrap();
            (active.message_id.clone(), active.body.clone())
        };
        let extended = self.fmt.extend_body(&active_body, batch);
        let extended_size = self.fmt.measure(&extended);
        if extended_size <= self.fmt.size_limit() {
            match self.sink.edit(&self.channel, &active_id, &extended).await {
                Ok(()) => {
                    if let Some(active) = self.active.as_mut() {
                        active.body = extended;
                    }
                    return;
                }
                Err(e) => {
                    error!(
                        "failed to extend active scroll buffer id={active_id} (size {extended_size}, limit {}): {e}; rolling over",
                        self.fmt.size_limit()
                    );
                }
            }
        } else {
            debug!(
                "active scroll buffer id={active_id} would exceed {} units (size {extended_size}); rolling over",
                self.fmt.size_limit()
            );
        }

        // Rollover: seal the current active, open a fresh one with the batch.
        let locked = self.active.take().unwrap();
        self.lock_active(locked).await;
        let body = self.fmt.scroll_body(batch);
        match self.sink.post(&self.channel, &body).await {
            Ok(ts) => {
                debug!("opened new active scroll buffer id={ts} after rollover");
                self.active = Some(ActiveScrollBuffer {
                    message_id: ts,
                    body,
                });
            }
            Err(e) => error!("failed to post fresh active scroll buffer after rollover: {e}"),
        }
    }

    /// Pop leading rows (oldest first) accumulating their rendered lines until
    /// the next line wouldn't fit in `budget` or the queue front is no longer
    /// a row. Leftover rows stay queued.
    fn drain_rows_into_budget(&mut self, budget: usize) -> String {
        let mut accum = String::new();
        let mut size = 0usize;
        while let Some(front) = self.peek_front_row() {
            let line_size = self.row_size(front);
            if size + line_size > budget {
                break;
            }
            let row = self.pop_front_row().unwrap();
            for c in row {
                accum.push(c);
            }
            accum.push('\n');
            size += line_size;
        }
        accum
    }

    /// Size of a row including its trailing newline, in the platform's unit.
    fn row_size(&self, row: &[char]) -> usize {
        let mut s = String::with_capacity(row.len() + 1);
        s.extend(row.iter());
        s.push('\n');
        self.fmt.measure(&s)
    }

    /// Largest byte index `<= s.len()` whose prefix measures `<= units` and is
    /// a valid char boundary. Used to hard-split an over-wide row.
    fn boundary_at(&self, s: &str, units: usize) -> usize {
        let mut consumed = 0usize;
        for (i, c) in s.char_indices() {
            let mut buf = [0u8; 4];
            let next = consumed + self.fmt.measure(c.encode_utf8(&mut buf));
            if next > units {
                return i;
            }
            consumed = next;
        }
        s.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Records every post/edit in order so tests can assert on the transcript.
    #[derive(Default)]
    struct MockSink {
        ops: StdMutex<Vec<String>>,
        next_id: AtomicUsize,
    }

    #[async_trait]
    impl TranscriptSink for MockSink {
        async fn post(&self, _channel: &str, text: &str) -> Result<String, BridgeError> {
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            self.ops.lock().unwrap().push(format!("POST#{id}: {text}"));
            Ok(format!("id{id}"))
        }
        async fn edit(&self, _channel: &str, id: &str, text: &str) -> Result<(), BridgeError> {
            self.ops.lock().unwrap().push(format!("EDIT {id}: {text}"));
            Ok(())
        }
    }

    impl MockSink {
        fn ops(&self) -> Vec<String> {
            self.ops.lock().unwrap().clone()
        }
    }

    /// ASCII-only test format: size == char count, simple labels/fences so
    /// assertions read cleanly.
    struct TestFormat {
        limit: usize,
    }

    impl TranscriptFormat for TestFormat {
        fn size_limit(&self) -> usize {
            self.limit
        }
        fn measure(&self, s: &str) -> usize {
            s.chars().count()
        }
        fn scroll_body(&self, rows_text: &str) -> String {
            format!("SB[{rows_text}]")
        }
        fn lock_body(&self, active_body: &str) -> String {
            active_body.replacen("SB[", "HIST[", 1)
        }
        fn extend_body(&self, existing: &str, new_rows_text: &str) -> String {
            // existing is "SB[<rows>]" — splice before the closing ']'.
            let trimmed = existing.strip_suffix(']').unwrap_or(existing);
            format!("{trimmed}{new_rows_text}]")
        }
        fn paginate(&self, body: &str) -> Vec<String> {
            // Simple char-count pagination on `\n` boundaries; mirrors what a
            // real format does, just without fences/headers.
            if body.chars().count() <= self.limit {
                return vec![body.to_string()];
            }
            let mut pages = Vec::new();
            let mut cur = String::new();
            for line in body.split_inclusive('\n') {
                if cur.chars().count() + line.chars().count() > self.limit && !cur.is_empty() {
                    pages.push(std::mem::take(&mut cur));
                }
                cur.push_str(line);
            }
            if !cur.is_empty() {
                pages.push(cur);
            }
            pages
        }
    }

    fn fmt(limit: usize) -> Box<dyn TranscriptFormat> {
        Box::new(TestFormat { limit })
    }

    fn row(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    // Drive the dispatcher to quiescence: feed messages, drop the sender, and
    // await the task so every queued unit is flushed.
    async fn run_to_completion(
        sink: std::sync::Arc<MockSink>,
        format: Box<dyn TranscriptFormat>,
        feed: impl FnOnce(&DispatchHandle),
    ) -> Vec<String> {
        let handle = spawn("chan".to_string(), sink.clone(), format);
        feed(&handle);
        handle.shutdown().await;
        sink.ops()
    }

    #[tokio::test]
    async fn live_frames_coalesce_to_latest() {
        let sink = std::sync::Arc::new(MockSink::default());
        let ops = run_to_completion(sink, fmt(1000), |h| {
            for i in 0..5 {
                h.try_send_frame(
                    vec![],
                    Some(LiveFrame {
                        text: format!("frame {i}"),
                        is_edit: true,
                    }),
                );
            }
        })
        .await;
        // All five queued before the task ran a post; only the newest posts.
        assert_eq!(ops.len(), 1, "expected one coalesced post, got {ops:?}");
        assert!(ops[0].contains("frame 4"), "got {ops:?}");
    }

    #[tokio::test]
    async fn streaming_chunks_all_post_in_order() {
        let sink = std::sync::Arc::new(MockSink::default());
        let ops = run_to_completion(sink, fmt(1000), |h| {
            for i in 0..3 {
                h.try_send_frame(
                    vec![],
                    Some(LiveFrame {
                        text: format!("chunk {i}"),
                        is_edit: false,
                    }),
                );
            }
        })
        .await;
        assert_eq!(ops.len(), 3, "streaming chunks must not coalesce: {ops:?}");
        assert!(ops[0].contains("chunk 0"));
        assert!(ops[2].contains("chunk 2"));
    }

    #[tokio::test]
    async fn scrolled_rows_open_active_buffer() {
        let sink = std::sync::Arc::new(MockSink::default());
        let ops = run_to_completion(sink, fmt(1000), |h| {
            h.try_send_frame(vec![row("alpha"), row("beta")], None);
        })
        .await;
        // One scroll-buffer message containing both rows.
        assert_eq!(ops.len(), 1, "got {ops:?}");
        assert!(ops[0].starts_with("POST"));
        assert!(ops[0].contains("alpha"));
        assert!(ops[0].contains("beta"));
    }

    #[tokio::test]
    async fn active_buffer_extends_then_rolls_over() {
        let sink = std::sync::Arc::new(MockSink::default());
        // Tight limit so the second batch can't fit and forces a rollover.
        // "SB[]" overhead is 4; each row "rowN\n" is 5 chars.
        let ops = run_to_completion(sink, fmt(12), |h| {
            // First frame: one row → opens active.
            h.try_send_frame(vec![row("row0")], None);
            // Second frame: another row → extends if it fits, else rolls.
            h.try_send_frame(vec![row("row1")], None);
            // Third: another → forces a fresh active after the lock.
            h.try_send_frame(vec![row("row2")], None);
        })
        .await;
        // Expect: at least one HIST[ lock (rollover) and multiple SB[ posts.
        let locks = ops.iter().filter(|o| o.contains("HIST[")).count();
        let posts = ops.iter().filter(|o| o.starts_with("POST")).count();
        assert!(locks >= 1, "expected a rollover lock, got {ops:?}");
        assert!(posts >= 2, "expected multiple scroll messages, got {ops:?}");
        // No row is ever lost.
        let joined = ops.join(" ");
        assert!(joined.contains("row0"));
        assert!(joined.contains("row1"));
        assert!(joined.contains("row2"));
    }

    #[tokio::test]
    async fn anchor_seals_active_and_resets_live() {
        let sink = std::sync::Arc::new(MockSink::default());
        let ops = run_to_completion(sink, fmt(1000), |h| {
            // Open an active scroll buffer.
            h.try_send_frame(vec![row("history line")], None);
            // Anchor: should seal it as HIST[.
            h.anchor();
            // New live frame after anchor posts fresh.
            h.try_send_frame(
                vec![],
                Some(LiveFrame {
                    text: "live after anchor".to_string(),
                    is_edit: true,
                }),
            );
        })
        .await;
        let joined = ops.join(" | ");
        assert!(
            joined.contains("HIST["),
            "anchor should seal active: {joined}"
        );
        assert!(joined.contains("live after anchor"));
    }

    #[tokio::test]
    async fn live_frame_edits_in_place_after_first_post() {
        let sink = std::sync::Arc::new(MockSink::default());
        // Feed two *separate* frames with a gap so the task posts the first
        // before the second arrives — the second should be an EDIT.
        let h = spawn("chan".to_string(), sink.clone(), fmt(1000));
        h.try_send_frame(
            vec![],
            Some(LiveFrame {
                text: "first".to_string(),
                is_edit: true,
            }),
        );
        // Let the dispatcher post "first".
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        h.try_send_frame(
            vec![],
            Some(LiveFrame {
                text: "second".to_string(),
                is_edit: true,
            }),
        );
        h.shutdown().await;

        let ops = sink.ops();
        assert!(
            ops.iter()
                .any(|o| o.starts_with("POST") && o.contains("first")),
            "got {ops:?}"
        );
        assert!(
            ops.iter()
                .any(|o| o.starts_with("EDIT") && o.contains("second")),
            "got {ops:?}"
        );
    }

    #[tokio::test]
    async fn empty_snapshot_is_noop() {
        let sink = std::sync::Arc::new(MockSink::default());
        let ops = run_to_completion(sink, fmt(1000), |h| {
            assert!(h.try_send_frame(vec![], None));
        })
        .await;
        assert!(
            ops.is_empty(),
            "empty snapshot should post nothing: {ops:?}"
        );
    }

    #[tokio::test]
    async fn overlong_live_frame_posts_last_page_only() {
        // A live frame larger than the limit must post a single message (it's
        // edited in place) showing the *tail* — never an over-limit body.
        let sink = std::sync::Arc::new(MockSink::default());
        let limit = 20;
        let big: String = (0..10).map(|i| format!("line{i}\n")).collect();
        let ops = run_to_completion(sink, fmt(limit), |h| {
            h.try_send_frame(
                vec![],
                Some(LiveFrame {
                    text: big.clone(),
                    is_edit: true,
                }),
            );
        })
        .await;
        assert_eq!(ops.len(), 1, "live frame is one message: {ops:?}");
        // The posted body fits the limit and is the tail (contains the last
        // line, not the first).
        let posted = &ops[0];
        let body = posted.strip_prefix("POST#0: ").unwrap();
        assert!(body.chars().count() <= limit, "over limit: {body:?}");
        assert!(body.contains("line9"), "should show the tail: {body:?}");
        assert!(
            !body.contains("line0"),
            "tail page shouldn't include the head: {body:?}"
        );
    }

    #[tokio::test]
    async fn overlong_streaming_chunk_posts_all_pages() {
        // A streaming chunk that overflows splits into multiple ordered posts,
        // none over the limit, with all content preserved.
        let sink = std::sync::Arc::new(MockSink::default());
        let limit = 20;
        let big: String = (0..10).map(|i| format!("row{i}\n")).collect();
        let ops = run_to_completion(sink, fmt(limit), |h| {
            h.try_send_frame(
                vec![],
                Some(LiveFrame {
                    text: big.clone(),
                    is_edit: false,
                }),
            );
        })
        .await;
        assert!(ops.len() > 1, "expected multiple pages: {ops:?}");
        let mut rejoined = String::new();
        for op in &ops {
            let body = op.split_once(": ").unwrap().1;
            assert!(body.chars().count() <= limit, "page over limit: {body:?}");
            rejoined.push_str(body);
        }
        assert_eq!(rejoined, big, "no streaming content may be lost");
    }
}
