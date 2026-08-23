//! The two palettes: ⌘⇧P runs a command, ⌘P goes to something.
//!
//! One overlay, two lists. The shell — the query field, the filtering, ↑/↓
//! selection, ↩ confirm, escape and click-away dismissal — is shared; what
//! differs is only what [`Workspace::palette_rows`] returns and what
//! [`Workspace::confirm_palette`] does with the row. Two surfaces that behave
//! differently under the same keystrokes would be two things to learn.
//!
//! Four details are load-bearing and easy to undo by accident.
//!
//! - **↑/↓ have to out-rank gpuikit's input bindings, and there is exactly one
//!   way to do it.** With the query field focused the context stack is
//!   `[Workspace, Palette, Input]`, so a binding in the bare `Palette` context
//!   (depth 2) loses to gpuikit's `Input` one (depth 3).
//!   `KeyBindingContextPredicate::depth_of` reports `A > B` at `B`'s depth, so
//!   `"Palette > Input"` *ties* on depth — and `bindings_for_input` breaks a
//!   depth tie on binding index, later wins. That is why [`bind_keys`] must run
//!   **after** `gpuikit::input::bind_input_keys`. Reorder those two calls in
//!   `main` and ↑/↓ silently go back to moving the cursor.
//! - **↩ and escape avoid that fight entirely.** The query field is built with
//!   `SubmitOn::Enter`, so confirm arrives as `InputStateEvent::Submit`, and
//!   escape is gpuikit's own blur.
//! - **`InputStateEvent::Blur` is emitted from the input's *paint*** — gpuikit
//!   raises it inside `InputState::cursor_visible` — so it only ever arrives
//!   while the input is still on screen, and a click that moves focus nowhere
//!   produces no blur at all. Click-away therefore needs its own handler, on a
//!   backdrop that is a **sibling** of the panel (gpui delivers to the topmost
//!   element and bubbles through *ancestors*, so a click on the panel would
//!   never reach a backdrop that contained it) and that carries an `id`,
//!   because an id is what registers a hitbox.
//! - **The row cap is applied before the selection wraps, not at render.** ↓
//!   wraps over the row list; a list truncated on its way to the screen would
//!   let the selection walk onto rows nobody can see.

use std::ops::Range;

use gpui::prelude::*;
use gpui::{
    actions, div, px, App, Context, Entity, Focusable, FontWeight, HighlightStyle, KeyBinding,
    MouseButton, ScrollHandle, StyledText, Window,
};
use gpuikit::input::{InputState, SubmitOn};
use gpuikit::theme::{ActiveTheme, Themeable};
use tasks_client::api::models::TaskId;

use crate::commands::{Command, Facts, Selection, COMMANDS};
use crate::components::text_field;
use crate::workspace::Workspace;

actions!(
    palette,
    [
        /// ⌘⇧P — run a command from the registry.
        ShowCommandPalette,
        /// ⌘P — go to a task, a spec or a build.
        GoToAnything,
        /// Move the highlight down a row, wrapping.
        SelectNextRow,
        /// Move the highlight up a row, wrapping.
        SelectPrevRow
    ]
);

/// The keymap context the palette panel sets on itself.
pub const PALETTE_CONTEXT: &str = "Palette";

/// The predicate ↑/↓ are bound under: the palette's own query field, and
/// nothing else. Two spellings of one fact with [`PALETTE_CONTEXT`], three
/// lines apart on purpose — a future refactor should derive this from the
/// constant rather than restate it.
const PALETTE_INPUT: &str = "Palette > Input";

/// How many rows the panel will draw. Applied in [`Workspace::palette_rows`],
/// not at render: see the module docs.
const MAX_ROWS: usize = 20;

/// Bind ↑/↓ inside the palette's query field.
///
/// **Must run after `gpuikit::input::bind_input_keys`.** See the module docs;
/// this is a tie broken on registration order, not on specificity.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("down", SelectNextRow, Some(PALETTE_INPUT)),
        KeyBinding::new("up", SelectPrevRow, Some(PALETTE_INPUT)),
    ]);
}

/// Which palette is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteKind {
    /// ⌘⇧P: the action registry.
    Commands,
    /// ⌘P: tasks, specs and builds.
    Navigate,
}

impl PaletteKind {
    fn placeholder(self) -> &'static str {
        match self {
            PaletteKind::Commands => "Run a command…",
            PaletteKind::Navigate => "Go to a task, spec or build…",
        }
    }

    fn empty_message(self) -> &'static str {
        match self {
            PaletteKind::Commands => "No matching command",
            PaletteKind::Navigate => "Nothing matches",
        }
    }
}

/// The open palette. `None` on [`Workspace`] means neither is up.
///
/// The query *field* is not here — it is one `Entity<InputState>` the
/// workspace owns like every other composer, cleared on each open. What lives
/// here is the query as of the last frame, which is what tells a keystroke
/// from a no-op and is why the selection resets when the text moves.
pub struct PaletteState {
    pub kind: PaletteKind,
    pub selected: usize,
    pub query: String,
    pub scroll: ScrollHandle,
}

/// Where confirming a row goes.
pub enum PaletteTarget {
    /// Dispatch the command's action.
    Command(&'static Command),
    /// Select the task and go to the section it belongs in.
    Task(TaskId),
    /// The same, and open the inspector on its spec.
    Spec(TaskId),
    /// A build's pull request, when it has one.
    Build(Option<String>),
}

/// One row, as the panel will draw it.
pub struct PaletteRow {
    /// The row's element id. Derived from what the row *is* — a command's
    /// registry id, a task's id — never from its index, because rows reorder
    /// on every keystroke and an element id that moves under the pointer drops
    /// the hover it was showing.
    pub key: String,
    pub label: String,
    /// The key equivalent, or the task's state — what the row is, in the
    /// trailing slot. Replaced by [`Self::refusal`] when there is one.
    pub detail: Option<String>,
    /// Why choosing this row would do nothing. Greys the row and is what the
    /// banner says if it is chosen anyway.
    pub refusal: Option<String>,
    /// Char offsets in `label` the query matched, for highlighting.
    pub positions: Vec<usize>,
    pub target: PaletteTarget,
}

impl PaletteRow {
    /// The trailing text: the refusal if there is one, since a row that cannot
    /// run has something more useful to say than its shortcut.
    fn trailing(&self) -> Option<&str> {
        self.refusal.as_deref().or(self.detail.as_deref())
    }
}

// --- the matcher ---

/// A character that is matched at all.
const MATCH: i32 = 8;
/// …and begins a word.
const WORD_START: i32 = 14;
/// …and is the first character of the candidate.
const STRING_START: i32 = 20;
/// …and directly follows the previous matched character. Must exceed
/// [`WORD_START`]: every character of `"S t o p"` begins a word, so with the
/// other ordering a string of initials would beat the literal substring it
/// spells out.
const CONSECUTIVE: i32 = 16;
/// The most a single run of skipped characters can cost. Capped, so a long
/// candidate is not penalised into oblivion for one late match — but present,
/// because it is what makes `#873` find task 873 rather than a title with a
/// stray 8, 7 and 3 in it.
const MAX_GAP: usize = 8;

const NEG: i32 = i32::MIN / 4;

/// A candidate that matched, and how well.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub score: i32,
    /// Char offsets into the candidate, ascending.
    pub positions: Vec<usize>,
}

fn gap_penalty(gap: usize) -> i32 {
    -(gap.min(MAX_GAP) as i32)
}

/// Lowercase, one character in and one out — see [`match_query`].
fn fold_case(ch: char) -> char {
    ch.to_lowercase().next().unwrap_or(ch)
}

/// Whether the character at `ix` begins a word — the start of the candidate,
/// anything after a non-alphanumeric, or the upper half of a camelCase seam.
fn word_starts(chars: &[char]) -> Vec<bool> {
    chars
        .iter()
        .enumerate()
        .map(|(ix, ch)| match ix {
            0 => true,
            _ => {
                let prev = chars[ix - 1];
                !prev.is_alphanumeric() || (prev.is_lowercase() && ch.is_uppercase())
            }
        })
        .collect()
}

/// Score `query` against `candidate`, or `None` if the characters aren't all
/// there in order.
///
/// A `query × candidate` dynamic program rather than a greedy left-to-right
/// scan: a greedy scan takes the *first* place each character fits, which is
/// exactly how "the substring you typed" loses to "those letters, scattered".
/// Case-insensitive, whitespace in the query is ignored, and an empty query
/// matches everything at score 0 — so an unfiltered palette is the full list
/// in its natural order.
pub fn match_query(query: &str, candidate: &str) -> Option<Match> {
    let needle: Vec<char> = query
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .map(fold_case)
        .collect();
    if needle.is_empty() {
        return Some(Match {
            score: 0,
            positions: Vec::new(),
        });
    }
    // One folded char per source char, deliberately: `positions` indexes the
    // candidate, so a fold that expanded a character (ẞ, İ) would slide every
    // highlight after it off the character it is about.
    let source: Vec<char> = candidate.chars().collect();
    let haystack: Vec<char> = source.iter().copied().map(fold_case).collect();
    if haystack.len() < needle.len() {
        return None;
    }
    let starts = word_starts(&source);

    let bonus = |ix: usize| match ix {
        0 => STRING_START,
        _ if starts.get(ix).copied().unwrap_or(false) => WORD_START,
        _ => 0,
    };

    // `prev[j]`: the best score for matching the query up to the previous
    // character, with that character landing exactly on candidate `j`.
    let mut prev: Vec<i32> = vec![NEG; haystack.len()];
    let mut back: Vec<Vec<usize>> = Vec::with_capacity(needle.len());

    for (i, wanted) in needle.iter().enumerate() {
        let mut cur = vec![NEG; haystack.len()];
        let mut cur_back = vec![usize::MAX; haystack.len()];
        // The best predecessor far enough back that the gap penalty has
        // already bottomed out. Keeping it as a running maximum is what holds
        // this to O(query × candidate × MAX_GAP) instead of a cubic scan.
        let mut far = NEG;
        let mut far_at = usize::MAX;

        for j in 0..haystack.len() {
            if i > 0 && j > MAX_GAP && prev[j - MAX_GAP - 1] > far {
                far = prev[j - MAX_GAP - 1];
                far_at = j - MAX_GAP - 1;
            }
            if haystack[j] != *wanted {
                continue;
            }
            if i == 0 {
                cur[j] = MATCH + bonus(j) + gap_penalty(j);
                continue;
            }
            let mut best = match far > NEG {
                true => far + gap_penalty(MAX_GAP + 1),
                false => NEG,
            };
            let mut best_at = far_at;
            let near = j.saturating_sub(MAX_GAP);
            for (offset, score) in prev[near..j].iter().enumerate() {
                if *score <= NEG {
                    continue;
                }
                let j2 = near + offset;
                let step = score
                    + match j - j2 - 1 {
                        0 => CONSECUTIVE,
                        gap => gap_penalty(gap),
                    };
                if step > best {
                    best = step;
                    best_at = j2;
                }
            }
            if best <= NEG {
                continue;
            }
            cur[j] = best + MATCH + bonus(j);
            cur_back[j] = best_at;
        }

        back.push(cur_back);
        prev = cur;
    }

    let (end, score) = prev
        .iter()
        .enumerate()
        .filter(|(_, score)| **score > NEG)
        .max_by_key(|(ix, score)| (**score, std::cmp::Reverse(*ix)))
        .map(|(ix, score)| (ix, *score))?;

    let mut positions = vec![0; needle.len()];
    let mut at = end;
    for i in (0..needle.len()).rev() {
        positions[i] = at;
        if i > 0 {
            at = back[i][at];
        }
    }
    Some(Match { score, positions })
}

/// Char offsets to byte ranges over `text`, coalescing runs.
///
/// Two jobs in one pass, and both matter: `StyledText::with_highlights` slices
/// the string by bytes, so a highlight after a multi-byte character would cut
/// mid-char, and a run of adjacent characters is one range rather than five.
pub fn byte_ranges(text: &str, positions: &[usize]) -> Vec<Range<usize>> {
    if positions.is_empty() {
        return Vec::new();
    }
    let offsets: Vec<usize> = text
        .char_indices()
        .map(|(byte, _)| byte)
        .chain(std::iter::once(text.len()))
        .collect();

    let mut ranges: Vec<Range<usize>> = Vec::new();
    for &position in positions {
        let (Some(&start), Some(&end)) = (offsets.get(position), offsets.get(position + 1)) else {
            continue;
        };
        match ranges.last_mut() {
            Some(last) if last.end == start => last.end = end,
            _ => ranges.push(start..end),
        }
    }
    ranges
}

// --- the workspace's half ---

impl Workspace {
    /// The query field. Single-line, `SubmitOn::Enter` so ↩ arrives as a
    /// `Submit` event rather than as a keystroke ↑/↓ would have to out-rank.
    pub(crate) fn new_palette_input(cx: &mut App) -> Entity<InputState> {
        cx.new(|cx| InputState::new_singleline(cx).submit_on(SubmitOn::Enter))
    }

    /// Open `kind`, switch to it, or — if it is already up — close it.
    pub(crate) fn toggle_palette(
        &mut self,
        kind: PaletteKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .palette
            .as_ref()
            .is_some_and(|state| state.kind == kind)
        {
            self.close_palette(window, cx);
            return;
        }
        self.palette_input
            .update(cx, |input, cx| input.set_content("", cx));
        self.palette = Some(PaletteState {
            kind,
            selected: 0,
            query: String::new(),
            scroll: ScrollHandle::new(),
        });
        window.focus(&self.palette_input.focus_handle(cx), cx);
        cx.notify();
    }

    /// Put the palette away and hand focus back to the workspace.
    ///
    /// The state is cleared *before* the focus moves, so the blur this may
    /// provoke finds nothing left to close.
    pub(crate) fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.take().is_none() {
            return;
        }
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    pub(crate) fn palette_is_open(&self) -> bool {
        self.palette.is_some()
    }

    /// Move the highlight, wrapping at both ends over the *capped* row list.
    pub(crate) fn move_palette_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let (rows, _) = self.palette_rows(cx);
        let Some(state) = self.palette.as_mut() else {
            return;
        };
        if rows.is_empty() {
            state.selected = 0;
            return;
        }
        let len = rows.len() as isize;
        let next = (state.selected as isize + delta).rem_euclid(len);
        state.selected = next as usize;
        state.scroll.scroll_to_item(state.selected);
        cx.notify();
    }

    /// Act on the highlighted row.
    ///
    /// The rows are rebuilt here rather than read off the last frame: a
    /// keystroke does not promise a render between typing and pressing return,
    /// and acting on a row the human never saw is the one bug this surface
    /// cannot have.
    pub(crate) fn confirm_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (mut rows, _) = self.palette_rows(cx);
        let Some(selected) = self.palette.as_ref().map(|state| state.selected) else {
            return;
        };
        if selected >= rows.len() {
            return;
        }
        let row = rows.swap_remove(selected);
        // A greyed row stays greyed and the palette stays open: the reason is
        // already on the row, and closing would throw away a query the human
        // is probably about to correct. It still goes to the banner, which the
        // backdrop is transparent enough to leave readable — same convention
        // as the row menu's keyboard path.
        if let Some(reason) = row.refusal {
            self.report(format!("{} — {reason}", row.label), cx);
            return;
        }
        self.close_palette(window, cx);
        match row.target {
            PaletteTarget::Command(command) => window.dispatch_action((command.action)(), cx),
            // Selecting *is* navigating since the frame swap: the middle
            // column shows the task's tabs, spec included, wherever it sits
            // in the pipeline.
            PaletteTarget::Task(id) | PaletteTarget::Spec(id) => self.select_task(id, cx),
            PaletteTarget::Build(url) => match url {
                Some(url) => cx.open_url(&url),
                None => self.report("that build has no pull request yet", cx),
            },
        }
    }

    /// Track the query field, once per frame. Called from `Render::render`
    /// beside `sync_chat_list`.
    ///
    /// Typing moves the selection back to the top: the row under the highlight
    /// after three more characters has nothing to do with the row that was
    /// under it before them.
    pub(crate) fn sync_palette(&mut self, cx: &mut Context<Self>) {
        let query = self.palette_input.read(cx).content().to_string();
        if let Some(state) = self.palette.as_mut() {
            if state.query != query {
                state.query = query;
                state.selected = 0;
            }
        }
    }

    /// The rows the open palette shows, capped, plus how many matched in all.
    ///
    /// The cap is applied here rather than at render so the selection can
    /// never walk onto a row nobody can see.
    pub(crate) fn palette_rows(&self, cx: &App) -> (Vec<PaletteRow>, usize) {
        let Some(state) = self.palette.as_ref() else {
            return (Vec::new(), 0);
        };
        let query = state.query.as_str();
        let mut scored: Vec<(i32, PaletteRow)> = match state.kind {
            PaletteKind::Commands => self.command_rows(query, cx),
            PaletteKind::Navigate => self.navigate_rows(query, cx),
        };
        // Stable, so an empty query (everything at 0) leaves the natural order
        // alone: the registry's own order, and the server's for the working set.
        scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        let total = scored.len();
        let rows = scored
            .into_iter()
            .take(MAX_ROWS)
            .map(|(_, row)| row)
            .collect();
        (rows, total)
    }

    fn command_rows(&self, query: &str, cx: &App) -> Vec<(i32, PaletteRow)> {
        let facts = Facts {
            menu: self.menu_state(cx),
            selection: self.palette_selection(cx),
        };
        COMMANDS
            .iter()
            .filter(|command| command.in_palette)
            .filter_map(|command| {
                let label = command.palette_label(facts);
                let matched = match_query(query, &label)?;
                Some((
                    matched.score,
                    PaletteRow {
                        key: format!("command:{}", command.id),
                        label,
                        detail: command.rendered_key(),
                        refusal: command.refusal_for(facts).map(str::to_string),
                        positions: matched.positions,
                        target: PaletteTarget::Command(command),
                    },
                ))
            })
            .collect()
    }

    /// Tasks, specs and builds.
    ///
    /// A build row goes to its **pull request** rather than to a task: `GET
    /// /builds` returns no spec or task link (that join exists only on
    /// `GET /builds/{id}`), and the app has no per-build surface to land on
    /// either. An in-app destination wants the list endpoint to carry
    /// `spec_ids` — server work, and a separate issue.
    fn navigate_rows(&self, query: &str, cx: &App) -> Vec<(i32, PaletteRow)> {
        let state = self.app_state.read(cx);
        let mut rows = Vec::new();

        for task in &state.tasks {
            let label = format!("#{} {}", task.gh_issue_number, task.title);
            if let Some(matched) = match_query(query, &label) {
                rows.push((
                    matched.score,
                    PaletteRow {
                        key: format!("task:{}", task.id.as_str()),
                        label,
                        // The label only, deliberately: a palette row is
                        // keyboard-driven, so a hover definition would never
                        // be seen, and the pane it navigates to carries the
                        // gloss. What matters is that the *word* comes from
                        // one place — see `crate::vocabulary`.
                        detail: Some(crate::vocabulary::task_state(task.state).label),
                        refusal: None,
                        positions: matched.positions,
                        target: PaletteTarget::Task(task.id.clone()),
                    },
                ));
            }
        }

        for spec in &state.specs {
            let Some(task) = state.task(&spec.task_id) else {
                continue;
            };
            let label = format!("Spec #{} {}", task.gh_issue_number, task.title);
            if let Some(matched) = match_query(query, &label) {
                rows.push((
                    matched.score,
                    PaletteRow {
                        key: format!("spec:{}", spec.id.as_str()),
                        label,
                        detail: state
                            .latest_queue_entry(&task.id)
                            .filter(|item| item.entry.spec_id == spec.id)
                            .map(|item| {
                                crate::vocabulary::spec_queue_status(item.entry.status).label
                            }),
                        refusal: None,
                        positions: matched.positions,
                        target: PaletteTarget::Spec(task.id.clone()),
                    },
                ));
            }
        }

        for build in &state.builds {
            let url = build.pr_number.and_then(|number| {
                let project = state
                    .projects
                    .iter()
                    .find(|project| project.id == build.project_id)?;
                Some(format!(
                    "https://github.com/{}/{}/pull/{number}",
                    project.repo_owner, project.repo_name
                ))
            });
            let label = match build.pr_number {
                Some(number) => format!("Build {} · PR #{number}", build.branch),
                None => format!("Build {}", build.branch),
            };
            if let Some(matched) = match_query(query, &label) {
                rows.push((
                    matched.score,
                    PaletteRow {
                        key: format!("build:{}", build.id.as_str()),
                        label,
                        detail: Some(crate::vocabulary::build_status(build.status).label),
                        refusal: url.is_none().then(|| "no pull request yet".to_string()),
                        positions: matched.positions,
                        target: PaletteTarget::Build(url),
                    },
                ));
            }
        }

        rows
    }

    /// The selection, as a surface that *can* grey per row sees it.
    fn palette_selection(&self, cx: &App) -> Selection {
        match self.selected_task.as_ref() {
            None => Selection::None,
            Some(id) => match self.row_context(id, cx) {
                Some(context) => Selection::Task(context),
                // Selected, but gone from the working set — which is the same
                // answer as nothing being selected.
                None => Selection::None,
            },
        }
    }

    /// The overlay: a backdrop and a panel, siblings, over everything.
    pub(crate) fn render_palette(&self, cx: &mut Context<Self>) -> Option<[gpui::AnyElement; 2]> {
        let state = self.palette.as_ref()?;
        let theme = cx.theme().clone();
        let (rows, total) = self.palette_rows(cx);
        let selected = state.selected;

        // Transparent, deliberately: it is a click catcher, not a scrim. A dim
        // would hide the sidebar banner, which is exactly where a refused row
        // says why it was refused.
        let backdrop = div()
            // The id is not decoration: it is what registers a hitbox, and
            // without one these clicks fall straight through to the rows
            // underneath.
            .id("palette-backdrop")
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _event, window, cx| this.close_palette(window, cx)),
            )
            .into_any_element();

        let list =
            div()
                .id("palette-rows")
                .flex()
                .flex_col()
                .max_h(px(360.))
                .overflow_y_scroll()
                .track_scroll(&state.scroll)
                .children(rows.iter().enumerate().map(|(ix, row)| {
                    let highlighted = ix == selected;
                    let text_color = match row.refusal.is_some() {
                        true => theme.fg_disabled(),
                        false => theme.fg(),
                    };
                    div()
                        .id(gpui::SharedString::from(row.key.clone()))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        .px(px(12.))
                        .py(px(5.))
                        .cursor_pointer()
                        .when(highlighted, |el| el.bg(theme.surface_tertiary()))
                        .on_click(cx.listener(move |this, _event, window, cx| {
                            if let Some(state) = this.palette.as_mut() {
                                state.selected = ix;
                            }
                            this.confirm_palette(window, cx);
                        }))
                        .child(
                            div()
                                .flex_1()
                                .overflow_hidden()
                                .truncate()
                                .text_sm()
                                .text_color(text_color)
                                .child(StyledText::new(row.label.clone()).with_highlights(
                                    byte_ranges(&row.label, &row.positions).into_iter().map(
                                        |range| {
                                            (
                                                range,
                                                HighlightStyle {
                                                    color: Some(theme.accent()),
                                                    font_weight: Some(FontWeight::BOLD),
                                                    ..Default::default()
                                                },
                                            )
                                        },
                                    ),
                                )),
                        )
                        .children(row.trailing().map(|trailing| {
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(theme.fg_muted())
                                .child(trailing.to_string())
                        }))
                }));

        let dropped = total.saturating_sub(rows.len());
        let panel = div()
            .absolute()
            .top(px(96.))
            .left_0()
            .right_0()
            .flex()
            .flex_row()
            .justify_center()
            .child(
                div()
                    .key_context(PALETTE_CONTEXT)
                    // An id for the same reason the backdrop has one: it
                    // registers a hitbox, and a hitbox is what occludes the
                    // backdrop's. Without it a click on the panel's own chrome
                    // — its padding, its footer — would fall through and
                    // dismiss the thing being clicked.
                    .id("palette-panel")
                    // A click that lands on the panel rather than on the query
                    // field puts the caret back in it: the header's padding and
                    // the gap beside a short query are the easy misses, and a
                    // palette whose keystrokes go nowhere is indistinguishable
                    // from one that has stopped responding. Rows keep their own
                    // click — this fires on mouse *down*, so confirming a row
                    // still runs on the mouse up that follows.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _event, window, cx| {
                            window.focus(&this.palette_input.focus_handle(cx), cx);
                        }),
                    )
                    .w(px(560.))
                    .flex()
                    .flex_col()
                    .rounded(px(8.))
                    .bg(theme.surface())
                    .border_1()
                    .border_color(theme.border())
                    .overflow_hidden()
                    .child(
                        div()
                            .flex_none()
                            .px(px(10.))
                            .py(px(8.))
                            .border_b_1()
                            .border_color(theme.border_subtle())
                            .text_sm()
                            .child(text_field(
                                &self.palette_input,
                                state.kind.placeholder(),
                                cx,
                            )),
                    )
                    .map(|el| match rows.is_empty() {
                        true => el.child(
                            div()
                                .px(px(12.))
                                .py(px(8.))
                                .text_sm()
                                .text_color(theme.fg_muted())
                                .child(state.kind.empty_message()),
                        ),
                        false => el.child(div().py(px(4.)).child(list)),
                    })
                    .when(dropped > 0, |el| {
                        el.child(
                            div()
                                .px(px(12.))
                                .py(px(4.))
                                .border_t_1()
                                .border_color(theme.border_subtle())
                                .text_xs()
                                .text_color(theme.fg_muted())
                                .child(format!("{dropped} more — keep typing")),
                        )
                    }),
            )
            .into_any_element();

        Some([backdrop, panel])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(query: &str, candidate: &str) -> Option<i32> {
        match_query(query, candidate).map(|matched| matched.score)
    }

    fn positions(query: &str, candidate: &str) -> Vec<usize> {
        match_query(query, candidate).unwrap().positions
    }

    /// An empty query is not a filter: the palette opens on the full list, in
    /// whatever order the thing behind it is already in.
    #[test]
    fn an_empty_query_matches_everything_at_zero() {
        for candidate in ["Server: Stop Server…", "", "#873 Something"] {
            assert_eq!(score("", candidate), Some(0), "{candidate:?}");
        }
        assert!(positions("", "anything").is_empty());
        // Whitespace is not a query either.
        assert_eq!(score("  \t ", "View: Home"), Some(0));
    }

    #[test]
    fn a_query_whose_characters_are_absent_does_not_match() {
        assert_eq!(score("zzz", "View: Home"), None);
        // In order, or not at all.
        assert_eq!(score("emoh", "Home"), None);
        // A query longer than the candidate cannot fit in it.
        assert_eq!(score("homeward", "Home"), None);
    }

    #[test]
    fn matching_is_case_insensitive_and_ignores_query_whitespace() {
        assert_eq!(score("HOME", "View: Home"), score("home", "View: Home"));
        assert_eq!(score("go h", "View: Home"), score("goh", "View: Home"));
    }

    /// `CONSECUTIVE` must exceed `WORD_START`: every character of `"S t o p"`
    /// begins a word, so with the other ordering a string of initials would
    /// beat the literal substring it spells out.
    #[test]
    fn a_literal_substring_beats_a_string_of_initials() {
        // Same starting position and the same leading bonus in both, so the
        // only thing being compared is consecutive against word-start.
        let literal = score("stop", "Stop Server").unwrap();
        let initials = score("stop", "S T O P Server").unwrap();
        assert!(
            literal > initials,
            "literal {literal} should beat initials {initials}"
        );
        const { assert!(CONSECUTIVE > WORD_START) };
    }

    /// The capped gap penalty is what makes an issue number find its issue
    /// rather than a title with the same digits scattered through it.
    #[test]
    fn an_issue_number_finds_its_issue() {
        let exact = score("#873", "#873 Command palette").unwrap();
        let scattered = score("#873", "#801 a big 7 and a 3").unwrap();
        assert!(
            exact > scattered,
            "exact {exact} should beat scattered {scattered}"
        );
    }

    /// The start of the string outranks the middle of a word, which is what
    /// keeps the thing you were plainly naming at the top.
    #[test]
    fn an_earlier_match_outranks_a_later_one() {
        assert!(score("ho", "Home page").unwrap() > score("ho", "A short home").unwrap());
        // A word start outranks the middle of a word.
        assert!(score("s", "a stop").unwrap() > score("s", "assort").unwrap());
    }

    /// The dynamic program's whole point: it does not take the first place
    /// each character fits.
    #[test]
    fn the_matcher_finds_the_best_placement_not_the_first() {
        // A greedy scan takes the leading "a", stranding "bc" — and then has
        // to match them across the gap. The DP finds the contiguous "abc".
        let matched = match_query("abc", "a x abc").unwrap();
        assert_eq!(matched.positions, vec![4, 5, 6]);
    }

    #[test]
    fn positions_point_at_the_characters_that_matched() {
        assert_eq!(positions("home", "View: Home"), vec![6, 7, 8, 9]);
        assert_eq!(positions("vh", "View: Home"), vec![0, 6]);
    }

    /// A highlight that slices a multi-byte character panics in gpui's text
    /// layout; runs are coalesced in the same pass.
    #[test]
    fn byte_ranges_land_on_char_boundaries_and_coalesce_runs() {
        let text = "Résumé ok";
        let matched = match_query("sum", text).unwrap();
        let ranges = byte_ranges(text, &matched.positions);
        assert_eq!(ranges.len(), 1, "{ranges:?}");
        for range in &ranges {
            assert!(text.is_char_boundary(range.start), "{range:?}");
            assert!(text.is_char_boundary(range.end), "{range:?}");
        }
        assert_eq!(&text[ranges[0].clone()], "sum");

        // Non-adjacent matches stay separate ranges.
        let matched = match_query("ré", "Résumé").unwrap();
        assert_eq!(byte_ranges("Résumé", &matched.positions).len(), 1);
        let split = byte_ranges("Résumé", &[0, 5]);
        assert_eq!(split.len(), 2);
        assert_eq!(&"Résumé"[split[1].clone()], "é");
    }

    #[test]
    fn byte_ranges_of_nothing_is_nothing() {
        assert!(byte_ranges("Home", &[]).is_empty());
        // An offset past the end is dropped rather than panicking.
        assert!(byte_ranges("Home", &[99]).is_empty());
    }

    /// The trailing slot says why a row cannot run, in place of its shortcut:
    /// a greyed row has something more useful to say than its keystroke.
    #[test]
    fn a_refusal_takes_the_trailing_slot_from_the_shortcut() {
        let row = PaletteRow {
            key: "command:queue-selected".into(),
            label: "Task: Add to Queue".into(),
            detail: Some("⇧⌘U".into()),
            refusal: None,
            positions: Vec::new(),
            target: PaletteTarget::Build(None),
        };
        assert_eq!(row.trailing(), Some("⇧⌘U"));

        let refused = PaletteRow {
            refusal: Some("already queued".into()),
            ..row
        };
        assert_eq!(refused.trailing(), Some("already queued"));
    }

    /// Both palettes answer to the same keystrokes, so the shell can be one
    /// thing; only their placeholders and their empty states differ.
    #[test]
    fn each_palette_says_what_it_is_for() {
        for kind in [PaletteKind::Commands, PaletteKind::Navigate] {
            assert!(!kind.placeholder().is_empty());
            assert!(!kind.empty_message().is_empty());
        }
        assert_ne!(
            PaletteKind::Commands.placeholder(),
            PaletteKind::Navigate.placeholder()
        );
    }

    /// ↑/↓ are bound one context deeper than the bare palette, which is the
    /// only depth at which they tie with gpuikit's own arrow bindings — and a
    /// tie is what registration order then breaks in our favour.
    #[test]
    fn the_arrow_bindings_are_bound_inside_the_query_field() {
        assert_eq!(PALETTE_INPUT, format!("{PALETTE_CONTEXT} > Input"));
    }
}
