//! The chat pane's row model: durable conversation turns and the live tick's
//! trail of narration and tool calls, interleaved in arrival order.
//!
//! The orchestrator's feed already carries the ordering — `Delta` chunks come
//! in generation order and every invocation gets its own `Tool` frame — so a
//! turn that goes text → tool → text only collapses into one line because the
//! client has nowhere to put the sequence. This is that somewhere: a flat row
//! list the view renders straight down, where a concluded tick's rows stay on
//! screen instead of being overwritten by the next one.
//!
//! Deliberately gpui-free, so the ordering rules can be unit-tested without a
//! `Context`. Three invariants, from which everything else follows:
//!
//! 1. **The live trail is always the tail.** [`ChatLog::live_from`] marks
//!    where it begins, and is held equal to `rows.len()` whenever no tick is
//!    open — which makes "insert a durable turn above the trail" and "append
//!    it at the end" the same line of code. Tail-following then means
//!    "follow the tick", and every structural change is an append or an
//!    insert at one known point.
//! 2. **Consecutive tool calls coalesce** into the trailing `Tools` entry, so
//!    a dozen curls read as one step rather than a dozen rows.
//! 3. **Concluding a trail drops its trailing `Text` entry.** Per the feed's
//!    contract, the segment after the last tool call *is* the reply, and the
//!    reply arrives durably in `orchestrator_messages` — keeping both would
//!    print the answer twice. Everything before it (narration, tool groups)
//!    stays.
//!
//! Scrollback is **session-local**: a trail lives for as long as this app
//! process does, and a restart or reconnect collapses the conversation back
//! to the durable messages. The feed is documented as ephemeral, and writing
//! it down would be the wrong side of that contract.

/// Identity for one trail entry. Monotonic within a process and meaningless
/// outside it — nothing here is persisted.
pub type ChatEntryId = u64;

/// What a trail entry holds. `Tools` is a group, not a call: consecutive
/// invocations coalesce into one.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ChatEntryKind {
    Text(String),
    Tools(Vec<String>),
}

/// A row's stable identity, which is also its markdown-cache key and what the
/// list diff compares. Stable across content growth on purpose: an entry that
/// gains a token keeps its key, so it stays inside the diff's common prefix
/// and gets re-measured instead of re-spliced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChatRowKey {
    /// A durable turn, keyed by its server-assigned seq.
    Message(i64),
    /// A live-trail entry, keyed by its session-local id.
    Entry(ChatEntryId),
}

impl ChatRowKey {
    /// The markdown cache key for this row. Distinct per row — one cache
    /// serves chat, specs and task bodies, and a growing stream re-renders
    /// every frame, so a collision here would be the loudest one.
    pub fn markdown_key(self) -> String {
        match self {
            ChatRowKey::Message(seq) => format!("chat:{seq}"),
            ChatRowKey::Entry(id) => format!("chat:entry:{id}"),
        }
    }
}

#[derive(Debug)]
enum Row {
    /// A durable turn: its index into the caller's message list, plus the seq
    /// that keys it.
    Message { index: usize, seq: i64 },
    Entry {
        id: ChatEntryId,
        kind: ChatEntryKind,
    },
}

/// What one row is, borrowed for the length of a render.
pub enum ChatRowKind<'a> {
    /// Index into `AppState::orchestrator_messages`.
    Message(usize),
    Text(&'a str),
    Tools(&'a [String]),
}

pub struct ChatRow<'a> {
    pub key: ChatRowKey,
    /// The last row of an *open* trail — both "the reply being written, not
    /// narration" (so it renders at full contrast) and "the row whose height
    /// changes between frames" (so it is the one to re-measure).
    pub live_tail: bool,
    pub kind: ChatRowKind<'a>,
}

/// The row list. See the module docs for the invariants it maintains.
#[derive(Default)]
pub struct ChatLog {
    rows: Vec<Row>,
    /// Where the live trail begins. Equal to `rows.len()` when no tick is
    /// open, which is what makes an insert-above-the-trail and an append the
    /// same operation.
    live_from: usize,
    next_id: ChatEntryId,
}

impl ChatLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every row's key, in order — what the view diffs its list against.
    pub fn keys(&self) -> Vec<ChatRowKey> {
        self.rows
            .iter()
            .map(|row| match row {
                Row::Message { seq, .. } => ChatRowKey::Message(*seq),
                Row::Entry { id, .. } => ChatRowKey::Entry(*id),
            })
            .collect()
    }

    /// The row at `ix`, or `None` past the end — a virtualized list can ask
    /// for a row the state has moved past.
    pub fn row(&self, ix: usize) -> Option<ChatRow<'_>> {
        let row = self.rows.get(ix)?;
        // Only an *open* trail has a live tail: after `conclude_live`,
        // `live_from == rows.len()` and this is false for every row.
        let live_tail = self.live_from < self.rows.len() && ix + 1 == self.rows.len();
        Some(match row {
            Row::Message { index, seq } => ChatRow {
                key: ChatRowKey::Message(*seq),
                live_tail: false,
                kind: ChatRowKind::Message(*index),
            },
            Row::Entry { id, kind } => ChatRow {
                key: ChatRowKey::Entry(*id),
                live_tail,
                kind: match kind {
                    ChatEntryKind::Text(text) => ChatRowKind::Text(text),
                    ChatEntryKind::Tools(labels) => ChatRowKind::Tools(labels),
                },
            },
        })
    }

    /// Add a durable turn. It lands *above* any open trail, because a turn
    /// that arrives mid-tick is history the tick is still answering — and
    /// when no tick is open, `live_from == rows.len()` makes this an append.
    pub fn push_message(&mut self, index: usize, seq: i64) {
        self.rows
            .insert(self.live_from, Row::Message { index, seq });
        self.live_from += 1;
    }

    /// Open a trail at the current tail.
    pub fn begin_live(&mut self) {
        self.live_from = self.rows.len();
    }

    /// Nothing has arrived on the open trail yet — the view is waiting, not
    /// writing. Load-bearing for `Started` handling: a tick whose trail is
    /// empty is the one the send already opened.
    pub fn live_is_empty(&self) -> bool {
        self.live_from >= self.rows.len()
    }

    /// Append assistant text, coalescing into the trailing `Text` entry.
    /// An empty chunk is a no-op — it would otherwise open a zero-height row
    /// and make an unstarted trail look started.
    pub fn push_delta(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(Row::Entry {
            kind: ChatEntryKind::Text(existing),
            ..
        }) = self.trailing_entry_mut()
        {
            existing.push_str(text);
            return;
        }
        let id = self.take_id();
        self.rows.push(Row::Entry {
            id,
            kind: ChatEntryKind::Text(text.to_string()),
        });
    }

    /// Record a tool invocation, coalescing into the trailing `Tools` group.
    pub fn push_tool(&mut self, label: String) {
        if let Some(Row::Entry {
            kind: ChatEntryKind::Tools(labels),
            ..
        }) = self.trailing_entry_mut()
        {
            labels.push(label);
            return;
        }
        let id = self.take_id();
        self.rows.push(Row::Entry {
            id,
            kind: ChatEntryKind::Tools(vec![label]),
        });
    }

    /// Seal the trail: drop its trailing `Text` entry (that segment is the
    /// reply, which lands durably) and keep everything before it. Reports the
    /// row that went away, if one did.
    pub fn conclude_live(&mut self) -> Option<ChatRowKey> {
        let mut dropped = None;
        if !self.live_is_empty() {
            if let Some(Row::Entry {
                id,
                kind: ChatEntryKind::Text(_),
            }) = self.rows.last()
            {
                dropped = Some(ChatRowKey::Entry(*id));
                self.rows.pop();
            }
        }
        self.live_from = self.rows.len();
        dropped
    }

    /// The last row, but only when it belongs to the open trail — a durable
    /// message can never be appended to or coalesced into.
    fn trailing_entry_mut(&mut self) -> Option<&mut Row> {
        if self.live_is_empty() {
            return None;
        }
        self.rows.last_mut()
    }

    fn take_id(&mut self) -> ChatEntryId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// A readable shape of the log: `M{index}` for a durable turn, the text
    /// itself for a narration entry, `[a|b]` for a tool group.
    fn shape(log: &ChatLog) -> Vec<String> {
        (0..log.keys().len())
            .map(|ix| match log.row(ix).unwrap().kind {
                ChatRowKind::Message(index) => format!("M{index}"),
                ChatRowKind::Text(text) => text.to_string(),
                ChatRowKind::Tools(labels) => format!("[{}]", labels.join("|")),
            })
            .collect()
    }

    #[test]
    fn deltas_accumulate_into_one_entry() {
        let mut log = ChatLog::new();
        log.begin_live();
        log.push_delta("Look");
        log.push_delta("ing at ");
        log.push_delta("the queue");
        assert_eq!(shape(&log), vec!["Looking at the queue"]);
    }

    #[test]
    fn text_and_tools_interleave_in_arrival_order() {
        let mut log = ChatLog::new();
        log.begin_live();
        log.push_delta("Checking the queue.");
        log.push_tool("Bash: curl /spec-queue".into());
        log.push_delta("Two specs are pending.");
        assert_eq!(
            shape(&log),
            vec![
                "Checking the queue.",
                "[Bash: curl /spec-queue]",
                "Two specs are pending.",
            ]
        );
    }

    #[test]
    fn consecutive_tools_coalesce_into_one_group() {
        let mut log = ChatLog::new();
        log.begin_live();
        for n in 0..12 {
            log.push_tool(format!("Bash: curl /tasks/{n}"));
        }
        assert_eq!(log.keys().len(), 1);
        match log.row(0).unwrap().kind {
            ChatRowKind::Tools(labels) => assert_eq!(labels.len(), 12),
            _ => panic!("expected one tool group"),
        }
    }

    #[test]
    fn tools_split_by_text_stay_separate_groups() {
        let mut log = ChatLog::new();
        log.begin_live();
        log.push_tool("a".into());
        log.push_tool("b".into());
        log.push_delta("thinking");
        log.push_tool("c".into());
        assert_eq!(shape(&log), vec!["[a|b]", "thinking", "[c]"]);
    }

    #[test]
    fn concluding_drops_the_reply_segment_and_keeps_the_narration() {
        let mut log = ChatLog::new();
        log.begin_live();
        log.push_delta("Let me look.");
        log.push_tool("Bash: curl /tasks".into());
        log.push_delta("There are three queued tasks.");
        let dropped = log.conclude_live();
        assert!(dropped.is_some());
        assert_eq!(shape(&log), vec!["Let me look.", "[Bash: curl /tasks]"]);
    }

    #[test]
    fn concluding_a_tool_terminated_trail_keeps_every_row() {
        let mut log = ChatLog::new();
        log.begin_live();
        log.push_delta("Queueing it.");
        log.push_tool("Bash: curl -X POST /tasks/7/queue".into());
        assert!(log.conclude_live().is_none());
        assert_eq!(
            shape(&log),
            vec!["Queueing it.", "[Bash: curl -X POST /tasks/7/queue]"]
        );
    }

    #[test]
    fn a_toolless_tick_collapses_to_just_the_durable_reply() {
        let mut log = ChatLog::new();
        log.begin_live();
        log.push_delta("Nothing is queued right now.");
        log.conclude_live();
        assert!(log.keys().is_empty());
        // ...and the durable turn is what remains on screen.
        log.push_message(0, 41);
        assert_eq!(shape(&log), vec!["M0"]);
    }

    #[test]
    fn a_turn_landing_mid_tick_sits_above_the_trail() {
        let mut log = ChatLog::new();
        log.push_message(0, 10);
        log.begin_live();
        log.push_delta("Working on it.");
        log.push_tool("Bash: curl /builds".into());
        // A pipeline event arrives while the tick is still running.
        log.push_message(1, 11);
        assert_eq!(
            shape(&log),
            vec!["M0", "M1", "Working on it.", "[Bash: curl /builds]"]
        );
    }

    #[test]
    fn only_the_last_row_of_a_live_trail_is_the_tail() {
        let mut log = ChatLog::new();
        log.push_message(0, 1);
        log.begin_live();
        log.push_delta("narration");
        log.push_tool("t".into());
        log.push_delta("the answer");
        let tails: Vec<bool> = (0..log.keys().len())
            .map(|ix| log.row(ix).unwrap().live_tail)
            .collect();
        assert_eq!(tails, vec![false, false, false, true]);
        // Sealing the trail leaves no live tail at all.
        log.conclude_live();
        assert!((0..log.keys().len()).all(|ix| !log.row(ix).unwrap().live_tail));
    }

    #[test]
    fn keys_stay_stable_across_growth() {
        let mut log = ChatLog::new();
        log.begin_live();
        log.push_delta("par");
        let before = log.keys();
        log.push_delta("tial");
        assert_eq!(before, log.keys());
    }

    #[test]
    fn every_row_has_a_distinct_markdown_key() {
        let mut log = ChatLog::new();
        log.push_message(0, 1);
        log.push_message(1, 2);
        log.begin_live();
        log.push_delta("a");
        log.push_tool("t".into());
        log.push_delta("b");
        let keys: HashSet<String> = log.keys().iter().map(|key| key.markdown_key()).collect();
        assert_eq!(keys.len(), log.keys().len());
    }

    #[test]
    fn the_opening_backfill_lands_above_a_trail_that_started_first() {
        // A `Delta` can beat the first snapshot: the trail exists before any
        // durable turn is known.
        let mut log = ChatLog::new();
        log.begin_live();
        log.push_delta("already working");
        for (index, seq) in [(0, 7), (1, 8), (2, 9)] {
            log.push_message(index, seq);
        }
        assert_eq!(shape(&log), vec!["M0", "M1", "M2", "already working"]);
        assert!(log.row(3).unwrap().live_tail);
    }

    #[test]
    fn a_sealed_trail_survives_the_next_tick() {
        let mut log = ChatLog::new();
        log.begin_live();
        log.push_delta("first look");
        log.push_tool("Bash: curl /tasks".into());
        log.conclude_live();
        log.push_message(0, 3);
        log.begin_live();
        log.push_delta("second look");
        log.push_tool("Bash: curl /specs".into());
        assert_eq!(
            shape(&log),
            vec![
                "first look",
                "[Bash: curl /tasks]",
                "M0",
                "second look",
                "[Bash: curl /specs]",
            ]
        );
    }

    #[test]
    fn an_empty_trail_concludes_without_disturbing_history() {
        let mut log = ChatLog::new();
        log.push_message(0, 1);
        log.push_message(1, 2);
        log.begin_live();
        assert!(log.live_is_empty());
        assert!(log.conclude_live().is_none());
        assert_eq!(shape(&log), vec!["M0", "M1"]);
    }

    #[test]
    fn an_empty_delta_does_not_open_a_row() {
        let mut log = ChatLog::new();
        log.begin_live();
        log.push_delta("");
        assert!(log.live_is_empty());
        assert_eq!(log.keys().len(), 0);
    }
}
