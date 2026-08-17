//! The root workspace view.
//!
//! Follows Zed's workspace/dock split: the workspace owns UI state
//! (active section, per-sidebar open/width, selection) and registers action
//! handlers; chrome components (`TitleBar`, `Sidebar`) are presentation-only
//! and talk back by dispatching actions, never by reaching into workspace
//! state. Server state lives in [`AppState`]; the workspace observes it and
//! re-renders.

use std::cell::RefCell;
use std::collections::HashSet;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    actions, div, list, px, App, ClipboardItem, Context, Div, Entity, FocusHandle, Focusable,
    FollowMode, ListAlignment, ListState, MouseButton, SharedString, Stateful, WeakEntity, Window,
    WindowHandle,
};
use gpuikit::elements::context_menu::{menu_item, MenuItems};
use gpuikit::elements::icon_button::icon_button;
use gpuikit::elements::input::text_area;
use gpuikit::elements::kbd::kbd;
use gpuikit::elements::loading_indicator::loading_indicator;
use gpuikit::elements::popover::{popover, PopoverState};
use gpuikit::elements::tooltip::tooltip;
use gpuikit::input::{InputState, InputStateEvent, SubmitOn};
use gpuikit::theme::{ActiveTheme, Themeable};
use gpuikit::DefaultIcons as Icons;
use tasks_client::api::models::{
    BuildStatus, ChatRole, CloseReason, Mode, ProjectId, ProjectStatus, SessionStatus, SpecId,
    SpecQueueStatus, TaskId, TaskState,
};

use crate::chat_log::{ChatEntryId, ChatRowKey, ChatRowKind};
use crate::commands::WORKSPACE_CONTEXT;
use crate::components::{
    markdown_block, sidebar, title_bar, MarkdownCache, SidebarSide, SidebarState,
};
use crate::issue_composer::{self, IssueComposer};
use crate::menus::{self, MenuState};
use crate::nav::{MiddleView, NavHistory, TaskTab};
use crate::palette::{
    GoToAnything, PaletteKind, PaletteState, SelectNextRow, SelectPrevRow, ShowCommandPalette,
};
use crate::projects::{self, ProjectFilter};
use crate::repo_composer::{self, RepoComposer};
use crate::row_menu::{self, RowAction, RowContext, RowEntry};
use crate::server::ServerControl;
use crate::state::AppState;
use crate::time;

pub(crate) const FONT: &str = "Menlo";

/// Reading-width cap for conversation content — long markdown replies
/// wrap at a comfortable measure instead of spanning a wide window.
const CHAT_MAX_WIDTH: gpui::Pixels = px(768.);

actions!(
    workspace,
    [
        ToggleLeftDock,
        ToggleRightDock,
        NewIssue,
        AddRepo,
        Dismiss,
        HistoryBack,
        HistoryForward,
        SetModePlay,
        SetModePause,
        SetModeStop,
        ToggleShowDone,
        // The Task menu's three verbs. They act on the selected row, so they
        // are element-handled like the dock toggles: with no workspace
        // focused there is no selection to act on, and greying out says so.
        QueueSelectedTask,
        ScoutSelectedTask,
        ApproveSelectedSpec
    ]
);

/// One chat row, owned, projected out of the app state before the markdown
/// cache needs `cx` mutably. `Empty` covers a row the state has moved past —
/// a virtualized list can ask for one, and a blank row beats a panic inside
/// a list item.
enum ChatRowView {
    Message {
        seq: i64,
        role: ChatRole,
        created_at: chrono::DateTime<chrono::Utc>,
        content: String,
    },
    /// A trail text segment. `live` marks the reply being written, as opposed
    /// to narration a tool call has already closed off.
    Text {
        key: String,
        text: String,
        live: bool,
    },
    Tools {
        id: ChatEntryId,
        labels: Vec<String>,
    },
    Empty,
}

pub struct Workspace {
    /// The root's own focus handle.
    ///
    /// Without one, `key_context("Workspace")` on the root was decorative:
    /// gpui falls back to the root dispatch node when the focused handle is
    /// absent from the rendered frame, and the *root* node carries no context,
    /// so the stack came out empty and every `Some("Workspace")` binding was
    /// dead at rest (#902). `Workspace::new` used to focus the chat composer,
    /// which only exists while the Chat section is rendered — which is exactly
    /// the frame in which the context vanished. Everything that moves focus
    /// away has to be able to hand it back here, which is also what the
    /// palette needs when it closes.
    pub(crate) focus_handle: FocusHandle,
    /// The middle column's position and history — the whole of "where am I"
    /// since the v3 frame swap retired sections.
    pub(crate) nav: NavHistory,
    /// Which of the selected task's tabs is showing. Reset to Overview on
    /// every navigation; meaningless while the catalog is up.
    pub(crate) task_tab: TaskTab,
    pub(crate) left_sidebar: SidebarState,
    pub(crate) right_sidebar: SidebarState,
    /// Which sidebar is currently being drag-resized, if any.
    pub(crate) resizing: Option<SidebarSide>,
    pub(crate) app_state: Entity<AppState>,
    /// The server *process*, as the Server menu acts on it. Observed for the
    /// same reason `AppState` is: the menu's shape and the sidebar's banner
    /// both depend on whether a `tasks` run is in flight.
    pub(crate) server_control: Entity<ServerControl>,
    /// Task shown in the inspector (right sidebar).
    pub(crate) selected_task: Option<TaskId>,
    /// The preserved bundle whose Delete has been armed by a first click, if
    /// any. Two clicks because there is no undo and no second copy: the file
    /// is the whole of an implementation whose branch never landed.
    ///
    /// Disarmed by any selection at all — a row click, the ✕, escape — rather
    /// than only by a *change* of selection. An armed button that outlives a
    /// click elsewhere is a trap: the second click would land on a different
    /// task's only copy, and re-arming costs one click.
    pub(crate) bundle_delete_armed: Option<tasks_client::api::models::BuildId>,
    /// Whether the Tasks list shows its archive of done tasks. Per-window and
    /// resets on relaunch: the app has no settings store, and a view filter
    /// that states its own count in a footer does not need one.
    pub(crate) show_done: bool,
    /// Chat composer.
    pub(crate) input: Entity<InputState>,
    /// Review-form composer in the inspector — feedback for a re-scout or a
    /// question for the orchestrator, depending on which button submits it.
    pub(crate) review_input: Entity<InputState>,
    /// Build-now composer in the inspector — the *rationale* for skipping the
    /// Scout on a task whose issue body already is the spec. Separate from
    /// [`Self::review_input`] because the two forms are never shown together
    /// (one is for a task before any work, the other for a spec awaiting a
    /// verdict) and sharing one draft between them would carry text meant for
    /// a reviewer into a decision record.
    pub(crate) build_input: Entity<InputState>,
    /// Issue-draft composer shown in the cmd-n window. Owned here, not by
    /// the window, so a dismissed draft survives to the next cmd-n.
    pub(crate) issue_input: Entity<InputState>,
    /// The left rail's task composer — the second door into the ⌘N flow.
    /// Its own draft, not [`Self::issue_input`]'s: the two surfaces can be
    /// on screen at once, and one `InputState` rendered in two places would
    /// fight over focus and caret.
    pub(crate) rail_input: Entity<InputState>,
    /// The cmd-n "new issue" window, if it has been opened. May be stale
    /// (window closed) — probed with `update` before re-fronting.
    issue_window: Option<WindowHandle<IssueComposer>>,
    /// Scroll state for the chat list — tail-following, so the view opens
    /// at the newest message and stays pinned while new ones land.
    chat_list: ListState,
    /// The row keys the list is currently synced to — what the next frame's
    /// keys are diffed against.
    chat_keys: Vec<ChatRowKey>,
    /// Tick revision the trailing row was last measured at.
    chat_tick_revision: u64,
    /// Tool groups the human has opened. Ids are session-local and never
    /// reused, so a stale one is inert.
    expanded_tools: HashSet<ChatEntryId>,
    /// Parsed-markdown entities for every reading surface, so re-renders
    /// don't re-parse. `RefCell` because most render paths hold `&self`.
    markdown: RefCell<MarkdownCache>,
    /// A row's context menu was opened and has not been chosen from or
    /// clicked away yet.
    ///
    /// gpuikit owns the menu itself (state, popup, dismissal), so this is not
    /// a duplicate of that state — it exists for exactly one thing: escape.
    /// Key bindings are dispatched *before* key-down listeners, so `escape`
    /// would reach this workspace's `Dismiss` handler first and throw away
    /// the selection the menu was about, while the menu — whose own escape
    /// handler never runs — stayed up. `Dismiss` checks this and gets out of
    /// the way instead. Cleared by the next mouse-down anywhere, which is
    /// also when the menu closes.
    row_menu_open: bool,
    /// The open palette (⌘⇧P or ⌘P), if one is up.
    pub(crate) palette: Option<PaletteState>,
    /// The palette's query field. Owned here like every other composer, and
    /// one field for both palettes: they never show together, and the query
    /// is cleared on each open anyway.
    pub(crate) palette_input: Entity<InputState>,
    /// Which repo this window is looking at. A view filter over the one working
    /// set, per-window and resetting on relaunch, exactly like [`Self::show_done`]
    /// — see [`crate::projects`] for why it is not a query parameter.
    pub(crate) project_filter: ProjectFilter,
    /// The title bar's repo switcher.
    ///
    /// A popover rather than gpuikit's `context_menu` (right-click only) and
    /// rather than a menu-bar menu: `set_menus` leaks a boxed action per item
    /// on every rebuild, and the item list here *is* the project list, so a bar
    /// that rebuilt on every add or archive would leak per repo per change.
    project_switcher: Entity<PopoverState>,
    /// `owner/repo` draft for the Add Repo window. Owned here, like the issue
    /// draft, so a dismissed one survives to the next open.
    pub(crate) repo_input: Entity<InputState>,
    /// The Add Repo window, if it has been opened. May be stale.
    repo_window: Option<WindowHandle<RepoComposer>>,
    /// A repo just added, by slug, waiting for a snapshot that holds it.
    ///
    /// By slug and not by id, because the client applies snapshots rather than
    /// responses: at the moment the Add Repo window closes there is no id yet.
    /// Cleared when it resolves, and by any other selection — an intent that
    /// outlived the human changing their mind would yank the window somewhere
    /// they did not ask to go.
    pending_repo_selection: Option<String>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let app_state = cx.new(AppState::new);
        let server_control = ServerControl::global(cx);
        // The menu bar joins three facts from two entities, so both observers
        // re-derive it. `sync` is a no-op unless something actually moved.
        cx.observe(&app_state, |this: &mut Self, _, cx| {
            this.sync_menus(cx);
            cx.notify();
        })
        .detach();
        cx.observe(&server_control, |this: &mut Self, _, cx| {
            this.sync_menus(cx);
            cx.notify();
        })
        .detach();

        // Live elapsed clocks (running scouts/builds, and the orchestrator
        // tick in flight) tick once a second — but only when something is
        // actually running. Either way the view re-renders every 30s so
        // relative timestamps ("5m") don't go stale in a quiet window.
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let mut ticks: u64 = 0;
            loop {
                executor.timer(Duration::from_secs(1)).await;
                ticks += 1;
                let alive = this
                    .update(cx, |this: &mut Workspace, cx| {
                        let state = this.app_state.read(cx);
                        let live = state
                            .sessions
                            .iter()
                            .any(|session| session.status == SessionStatus::Running)
                            || state
                                .builds
                                .iter()
                                .any(|build| build.status == BuildStatus::Running)
                            || state.orchestrator_tick.is_some()
                            // A restart is minutes of staged work with the
                            // event stream down for most of it; its clock in
                            // the banner is the only thing still moving.
                            || this.server_control.read(cx).busy();
                        if live || ticks.is_multiple_of(30) {
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !alive {
                    return;
                }
            }
        })
        .detach();

        let input = cx.new(|cx| {
            // Cmd-enter sends everywhere in this app; enter is a newline.
            let mut state = InputState::new_multiline(cx).submit_on(SubmitOn::CmdEnter);
            state.set_placeholder("Talk to the orchestrator…", cx);
            state
        });
        let review_input = cx.new(|cx| {
            // Compose convention: cmd-enter fires the primary action (Ask),
            // plain enter stays a newline — feedback is often multi-line.
            let mut state = InputState::new_multiline(cx).submit_on(SubmitOn::CmdEnter);
            state.set_placeholder("Feedback or a question about this spec…", cx);
            state
        });
        let build_input = cx.new(|cx| {
            let mut state = InputState::new_multiline(cx).submit_on(SubmitOn::CmdEnter);
            state.set_placeholder("Why this needs no spec…", cx);
            state
        });
        // The composers gate their submit buttons on content, so keystrokes
        // must re-render the workspace, not just the input element.
        cx.observe(&input, |_, _, cx| cx.notify()).detach();
        cx.observe(&review_input, |_, _, cx| cx.notify()).detach();
        cx.observe(&build_input, |_, _, cx| cx.notify()).detach();
        cx.subscribe(&build_input, |this, _, event: &InputStateEvent, cx| {
            if matches!(event, InputStateEvent::Submit) {
                this.build_selected_task_now(cx);
            }
        })
        .detach();
        cx.subscribe(&input, |this, _, event: &InputStateEvent, cx| {
            if matches!(event, InputStateEvent::Submit) {
                this.send_chat(cx);
            }
        })
        .detach();
        cx.subscribe(&review_input, |this, _, event: &InputStateEvent, cx| {
            if matches!(event, InputStateEvent::Submit) {
                this.ask_about_selected_spec(cx);
            }
        })
        .detach();
        // The issue composer window subscribes to this itself — its submit
        // and dismiss both live in that window's context.
        let issue_input = cx.new(|cx| {
            let mut state = InputState::new_multiline(cx).submit_on(SubmitOn::CmdEnter);
            state.set_placeholder("Describe the issue…", cx);
            state
        });
        // The rail composer: same flow as ⌘N (the orchestrator titles and
        // files), its own draft.
        let rail_input = cx.new(|cx| {
            let mut state = InputState::new_multiline(cx).submit_on(SubmitOn::CmdEnter);
            state.set_placeholder("Add a task…", cx);
            state
        });
        cx.observe(&rail_input, |_, _, cx| cx.notify()).detach();
        cx.subscribe(&rail_input, |this, _, event: &InputStateEvent, cx| {
            if matches!(event, InputStateEvent::Submit) {
                this.submit_rail_composer(cx);
            }
        })
        .detach();
        // One line, so cmd-enter and plain enter can mean the same thing —
        // there is no newline to protect.
        let repo_input = cx.new(|cx| {
            let mut state = InputState::new_singleline(cx).submit_on(SubmitOn::CmdEnter);
            state.set_placeholder("owner/repo", cx);
            state
        });

        // The switcher's rows *are* the project list, so both callbacks read
        // the workspace back out of a weak handle rather than closing over a
        // snapshot that would be stale by the first archive.
        let project_switcher = {
            let trigger = cx.entity().downgrade();
            let content = cx.entity().downgrade();
            cx.new(|_cx| {
                PopoverState::new(
                    popover("project-switcher")
                        .trigger(move |window, cx| {
                            Self::render_switcher_trigger(&trigger, window, cx)
                        })
                        .content(move |window, cx| {
                            Self::render_switcher_content(&content, window, cx)
                        }),
                )
            })
        };
        cx.observe(&project_switcher, |_, _, cx| cx.notify())
            .detach();

        // The palette's query field. ↩ confirms (hence `SubmitOn::Enter`),
        // escape is gpuikit's own blur, and the blur is how the palette learns
        // it was dismissed — but only when the input is still on screen to
        // paint it, which is why click-away has its own handler on the
        // backdrop.
        let palette_input = Self::new_palette_input(cx);
        cx.observe(&palette_input, |_, _, cx| cx.notify()).detach();
        cx.subscribe_in(
            &palette_input,
            window,
            |this, _, event: &InputStateEvent, window, cx| match event {
                InputStateEvent::Submit => this.confirm_palette(window, cx),
                InputStateEvent::Blur if this.palette_is_open() => this.close_palette(window, cx),
                _ => {}
            },
        )
        .detach();

        // Focus the root, not the chat composer: the composer is only
        // rendered in the Chat section, and a focused handle that is absent
        // from the frame drops the whole context stack — which is #902, and
        // which would leave every `Workspace`-context binding dead at rest.
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);

        // …and take it back whenever the focused element leaves the tree: an
        // input blurring itself on escape, a popup closing, a panel dismissed
        // with its composer focused. gpui runs this only when the window has
        // *nothing* focused and holds the result still for one draw, so it
        // cannot loop; the paths above that hand focus back explicitly do it a
        // frame earlier, and this is the backstop for the ones that don't —
        // including anything added later that forgets. It cannot cover
        // startup, whose guard requires a non-empty previous focus path, which
        // is why the line above is still explicit.
        cx.on_focus_lost(window, |this, window, cx| {
            window.focus(&this.focus_handle, cx);
        })
        .detach();

        // Follow the conversation tail: top-aligned with `FollowMode::Tail`
        // (not `ListAlignment::Bottom` — a short conversation should read
        // from the top). The pin releases when the user scrolls up and
        // re-engages within a pixel of the bottom. Overdraw is generous so
        // items near the viewport stay measured and the scroll doesn't pop.
        let chat_list = ListState::new(0, ListAlignment::Top, px(2048.));
        chat_list.set_follow_mode(FollowMode::Tail);
        {
            // Re-render so the jump-to-newest button tracks the pin state.
            // Deferred: the handler can run while the list's internals are
            // borrowed, so nothing here may touch the list synchronously.
            let workspace = cx.entity().downgrade();
            chat_list.set_scroll_handler(move |_event, _window, cx| {
                let workspace = workspace.clone();
                cx.defer(move |cx| {
                    if let Some(workspace) = workspace.upgrade() {
                        workspace.update(cx, |_, cx| cx.notify());
                    }
                });
            });
        }

        Self {
            focus_handle,
            nav: NavHistory::default(),
            task_tab: TaskTab::default(),
            left_sidebar: SidebarState::new(true),
            // The chat pane: on screen by default (the design's always-up
            // right column), and wide — it is a reading surface.
            right_sidebar: SidebarState::new(true).with_width(px(460.)),
            resizing: None,
            app_state,
            server_control,
            selected_task: None,
            bundle_delete_armed: None,
            show_done: false,
            input,
            review_input,
            build_input,
            issue_input,
            rail_input,
            issue_window: None,
            chat_list,
            chat_keys: Vec::new(),
            chat_tick_revision: 0,
            expanded_tools: HashSet::new(),
            markdown: RefCell::new(MarkdownCache::new()),
            row_menu_open: false,
            palette: None,
            palette_input,
            project_filter: ProjectFilter::All,
            project_switcher,
            repo_input,
            repo_window: None,
            pending_repo_selection: None,
        }
    }

    /// The shared parsed-markdown cache, borrowed mutably for the duration
    /// of one `entity` call. Render paths hold `&self`, hence the `RefCell`.
    pub(crate) fn markdown_cache(&self) -> std::cell::RefMut<'_, MarkdownCache> {
        self.markdown.borrow_mut()
    }

    fn sidebar_mut(&mut self, side: SidebarSide) -> &mut SidebarState {
        match side {
            SidebarSide::Left => &mut self.left_sidebar,
            SidebarSide::Right => &mut self.right_sidebar,
        }
    }

    fn toggle_sidebar(&mut self, side: SidebarSide, cx: &mut Context<Self>) {
        self.sidebar_mut(side).toggle();
        cx.notify();
    }

    /// Point the middle column somewhere. The one write to [`Self::nav`]
    /// outside the history steps, so selection, drafts and the tab strip stay
    /// consistent by construction rather than at each call site.
    pub(crate) fn navigate(&mut self, view: MiddleView, cx: &mut Context<Self>) {
        if self.nav.navigate(view) {
            self.settle_after_nav(cx);
        }
        cx.notify();
    }

    /// ⌘[ — step the middle column back.
    pub(crate) fn history_back(&mut self, cx: &mut Context<Self>) {
        if self.nav.back() {
            self.settle_after_nav(cx);
            cx.notify();
        }
    }

    /// ⌘] — step the middle column forward.
    pub(crate) fn history_forward(&mut self, cx: &mut Context<Self>) {
        if self.nav.forward() {
            self.settle_after_nav(cx);
            cx.notify();
        }
    }

    /// What every nav movement implies: the tab strip resets to the landing
    /// tab, an armed bundle Delete disarms, and — when the selection actually
    /// changed task — the per-task drafts are cleared, because a review is
    /// about one spec and a build-now rationale is the only record of why
    /// *that* task skipped its Scout.
    fn settle_after_nav(&mut self, cx: &mut Context<Self>) {
        self.task_tab = TaskTab::Overview;
        self.bundle_delete_armed = None;
        let selected = match self.nav.current() {
            MiddleView::Task(id) => Some(id.clone()),
            MiddleView::AllTasks => None,
        };
        if selected != self.selected_task {
            self.review_input
                .update(cx, |input, cx| input.set_content("", cx));
            self.build_input
                .update(cx, |input, cx| input.set_content("", cx));
            self.selected_task = selected;
        }
    }

    /// Show or hide the Tasks list's archive of done tasks — the list's
    /// footer button, `View ▸ Show Done Tasks` and `shift-cmd-d`, on one
    /// path. The menu's checkmark is part of [`MenuState`], so this has to
    /// re-derive the bar as well as re-render.
    pub(crate) fn toggle_show_done(&mut self, cx: &mut Context<Self>) {
        self.show_done = !self.show_done;
        self.sync_menus(cx);
        cx.notify();
    }

    /// Set the pipeline mode — the title bar's play/pause buttons and the
    /// Server menu's radio group, on one path.
    pub(crate) fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        self.app_state
            .update(cx, |state, cx| state.set_mode(mode, cx));
    }

    /// Rebuild the menu bar from the facts it depends on, which live in three
    /// places: `AppState` knows whether a server is answering and what mode it
    /// is in, `ServerControl` knows whether a `tasks` run is in flight, and
    /// the archive toggle is the workspace's own. Called from observers on
    /// both entities and from `toggle_show_done`; a no-op unless something
    /// moved.
    fn sync_menus(&self, cx: &mut Context<Self>) {
        let menu_state = self.menu_state(cx);
        menus::sync(cx, menu_state);
    }

    /// The facts the bar's shape depends on, read out of the two entities that
    /// hold them plus this window's own archive toggle.
    ///
    /// Extracted from [`Self::sync_menus`] because the command palette greys
    /// its rows against the same facts, and two derivations of "can this run?"
    /// is how the two surfaces stop agreeing.
    pub(crate) fn menu_state(&self, cx: &App) -> MenuState {
        let busy = self.server_control.read(cx).busy();
        let state = self.app_state.read(cx);
        MenuState {
            serving: state.connected,
            mode: state.mode,
            busy,
            show_done: self.show_done,
        }
    }

    // --- selection (called from section rows) ---

    /// Select a task — which, since the frame swap, *is* navigating the
    /// middle column to it. Re-selecting the current task is a no-op
    /// (`NavHistory::navigate` refuses the duplicate), which is also what
    /// keeps a right-click on the selected row from clearing its drafts.
    pub(crate) fn select_task(&mut self, id: TaskId, cx: &mut Context<Self>) {
        self.navigate(MiddleView::Task(id), cx);
    }

    /// Send a message into the orchestrator conversation. The chat pane is
    /// always on screen; `reveal` brings it back only if content-hiding put
    /// it away, without overriding a deliberate ⌘R dismissal… which sending a
    /// message is the one good reason to override — the reply would otherwise
    /// stream into a pane the human cannot see.
    pub(crate) fn ask_orchestrator(&mut self, message: String, cx: &mut Context<Self>) {
        self.app_state
            .update(cx, |state, cx| state.send_orchestrator_message(message, cx));
        self.right_sidebar.force_open();
        // Sending re-engages the tail pin even if the user had scrolled up —
        // their own message (and the reply behind it) lands at the bottom.
        self.chat_list.scroll_to_end();
        self.chat_list.set_follow_mode(FollowMode::Tail);
        cx.notify();
    }

    /// Submit the chat composer, if it has content.
    pub(crate) fn send_chat(&mut self, cx: &mut Context<Self>) {
        let content = self.input.read(cx).content().trim().to_string();
        if content.is_empty() {
            return;
        }
        self.input.update(cx, |input, cx| input.set_content("", cx));
        self.app_state
            .update(cx, |state, cx| state.send_orchestrator_message(content, cx));
        self.chat_list.scroll_to_end();
        self.chat_list.set_follow_mode(FollowMode::Tail);
    }

    /// Submit the review draft as a question about the selected task's
    /// pending spec — the review form's primary (cmd-enter) action. No-op
    /// without a selected task, a pending spec, or draft text.
    pub(crate) fn ask_about_selected_spec(&mut self, cx: &mut Context<Self>) {
        let Some((number, title, spec_id)) = ({
            let state = self.app_state.read(cx);
            self.selected_task
                .as_ref()
                .and_then(|id| state.task(id))
                .and_then(|task| {
                    let spec = state.latest_spec(&task.id)?;
                    state
                        .spec_queue
                        .iter()
                        .any(|item| {
                            item.entry.spec_id == spec.id
                                && item.entry.status == SpecQueueStatus::PendingReview
                        })
                        .then(|| (task.gh_issue_number, task.title.clone(), spec.id.clone()))
                })
        }) else {
            return;
        };
        let Some(text) = self.take_review_draft(cx) else {
            return;
        };
        let message = format!(
            "Re: task #{number} \"{title}\" — its spec ({spec_id}) is pending review.\n\n{text}"
        );
        self.ask_orchestrator(message, cx);
    }

    /// "Build now" for the selected task: write its issue body as the spec,
    /// approve it, and queue the build — the build-now form's primary
    /// (cmd-enter) action.
    ///
    /// No-op without a selected task in `backlog` or `queued`, and no-op
    /// without a rationale. The server does not demand one — only the
    /// orchestrator is ever gated on an explanation, and it cannot call this
    /// at all — but this is a one-click path to an *unreviewed* build, and the
    /// seconds saved by not typing why are not worth the record.
    pub(crate) fn build_selected_task_now(&mut self, cx: &mut Context<Self>) {
        let Some(id) = ({
            let state = self.app_state.read(cx);
            self.selected_task
                .as_ref()
                .and_then(|id| state.task(id))
                .filter(|task| matches!(task.state, TaskState::Backlog | TaskState::Queued))
                .map(|task| task.id.clone())
        }) else {
            return;
        };
        let Some(rationale) = self.take_build_draft(cx) else {
            return;
        };
        self.app_state
            .update(cx, |state, cx| state.build_task_now(id, rationale, cx));
    }

    /// The trimmed build-now rationale, clearing the composer — `None` if
    /// empty. Mirrors [`Self::take_review_draft`].
    pub(crate) fn take_build_draft(&mut self, cx: &mut Context<Self>) -> Option<String> {
        let text = self.build_input.read(cx).content().trim().to_string();
        if text.is_empty() {
            return None;
        }
        self.build_input
            .update(cx, |input, cx| input.set_content("", cx));
        Some(text)
    }

    // --- the row context menu ---

    /// What the menu's shape depends on, for the row about `id`. `None` when
    /// the task has left the working set between render and click.
    pub(crate) fn row_context(&self, id: &TaskId, cx: &App) -> Option<RowContext> {
        let state = self.app_state.read(cx);
        let task = state.task(id)?;
        Some(RowContext {
            task_state: task.state,
            gh_state: task.gh_state,
            has_github_url: state.github_url(task).is_some(),
            spec: state.latest_queue_entry(id).map(|item| item.entry.status),
        })
    }

    /// The menu a right-click on the row about `id` opens, as gpuikit's
    /// builder callback.
    ///
    /// Built fresh on every open, so it greys against the state at the moment
    /// of the click rather than the state of the last frame. An associated
    /// function rather than a method: the sections call it from inside their
    /// row closures, where a borrow of `self` would collide with the `cx` the
    /// same closure is already holding.
    pub(crate) fn row_menu(
        id: TaskId,
        cx: &Context<Self>,
    ) -> impl Fn(MenuItems, &mut Window, &mut App) -> MenuItems + 'static {
        let workspace = cx.entity().downgrade();
        move |menu, _window, cx| {
            let Some(entity) = workspace.upgrade() else {
                return menu;
            };
            let workspace = workspace.clone();
            let id = id.clone();
            entity.update(cx, move |this, cx| {
                // Right-click also selects. Half these verbs act on a spec
                // only the inspector renders, and a menu acting on a row you
                // cannot see is how you approve the wrong spec — it is also
                // what gives "Review Spec…" somewhere to land.
                this.select_task(id.clone(), cx);
                let Some(context) = this.row_context(&id, cx) else {
                    return menu;
                };
                this.row_menu_open = true;
                row_menu::entries(context)
                    .into_iter()
                    .fold(menu, |menu, entry| match entry {
                        RowEntry::Separator => menu.separator(),
                        RowEntry::Item(row) => {
                            let mut item =
                                menu_item(row.menu_label()).disabled(row.disabled.is_some());
                            if row.destructive {
                                item = item.destructive();
                            }
                            if let Some(shortcut) = row.kbd {
                                item = item.kbd(shortcut);
                            }
                            let action = row.action;
                            let id = id.clone();
                            let workspace = workspace.clone();
                            menu.item(item.on_click(move |window, cx| {
                                workspace
                                    .update(cx, |this, cx| {
                                        this.perform_row_action(action, id.clone(), window, cx);
                                    })
                                    .ok();
                            }))
                        }
                    })
            })
        }
    }

    /// Run one row verb, having re-checked that it can run.
    ///
    /// The check is not belt-and-braces: the menu greyed against the state at
    /// open time, and the keyboard path never saw a menu at all. A refusal
    /// goes to the banner — a verb that quietly does nothing reads as a bug,
    /// and the reason reads as an answer. The server is still the authority,
    /// so anything that gets past this comes back as its own message.
    pub(crate) fn perform_row_action(
        &mut self,
        action: RowAction,
        id: TaskId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.row_menu_open = false;
        let Some(context) = self.row_context(&id, cx) else {
            self.report("that task is no longer in the working set", cx);
            return;
        };
        if let Some(item) = row_menu::item(context, action) {
            if let Some(reason) = item.disabled {
                self.report(format!("{} — {reason}", item.label), cx);
                return;
            }
        }

        match action {
            RowAction::Queue => self
                .app_state
                .update(cx, |state, cx| state.queue_task(id, cx)),
            RowAction::Dequeue => self
                .app_state
                .update(cx, |state, cx| state.dequeue_task(id, cx)),
            RowAction::ScoutNow => self
                .app_state
                .update(cx, |state, cx| state.scout_task_now(id, cx)),
            RowAction::CancelRun => self
                .app_state
                .update(cx, |state, cx| state.cancel_run(id, cx)),
            RowAction::ApproveSpec => {
                if let Some(spec_id) = self.latest_spec_id(&id, cx) {
                    self.app_state.update(cx, |state, cx| {
                        state.review_spec(spec_id, SpecQueueStatus::Approved, None, cx)
                    });
                }
            }
            // The one verb that opens something, and the reason it ends in an
            // ellipsis: the verdict needs text, and the place to write it is
            // next to the spec it is about.
            RowAction::ReviewSpec => self.begin_review(id, window, cx),
            RowAction::RequestBuild => {
                if let Some(spec_id) = self.latest_spec_id(&id, cx) {
                    self.app_state
                        .update(cx, |state, cx| state.build_spec(spec_id, cx));
                }
            }
            RowAction::CloseCompleted => self.app_state.update(cx, |state, cx| {
                state.close_task(id, CloseReason::Completed, cx)
            }),
            RowAction::CloseNotPlanned => self.app_state.update(cx, |state, cx| {
                state.close_task(id, CloseReason::NotPlanned, cx)
            }),
            RowAction::Reopen => self
                .app_state
                .update(cx, |state, cx| state.reopen_task(id, cx)),
            RowAction::OpenOnGitHub => {
                if let Some(url) = self.github_url(&id, cx) {
                    cx.open_url(&url);
                }
            }
            RowAction::CopyNumber => {
                if let Some(number) = self
                    .app_state
                    .read(cx)
                    .task(&id)
                    .map(|task| task.gh_issue_number)
                {
                    // With the `#`, because that is the form that means
                    // something wherever it is being pasted.
                    cx.write_to_clipboard(ClipboardItem::new_string(format!("#{number}")));
                }
            }
            RowAction::CopyUrl => {
                if let Some(url) = self.github_url(&id, cx) {
                    cx.write_to_clipboard(ClipboardItem::new_string(url));
                }
            }
        }
    }

    /// Show the task with the review composer focused — "Review Spec…"
    /// landing where the spec text already is, rather than in a modal that
    /// would cover the thing being judged. The middle column is always
    /// visible, so unlike the inspector era there is no panel to force open;
    /// the form renders on the task's landing tab, which `select_task` just
    /// reset to.
    fn begin_review(&mut self, id: TaskId, window: &mut Window, cx: &mut Context<Self>) {
        self.select_task(id, cx);
        window.focus(&self.review_input.focus_handle(cx), cx);
        cx.notify();
    }

    /// Run a row verb against the selected row — the Task menu and its key
    /// equivalents. Menu-bar items cannot grey per selection (`set_menus`
    /// leaks a boxed action per item on every rebuild, and the selection moves
    /// on every arrow key), so the refusal is reported rather than shown in
    /// advance.
    pub(crate) fn run_on_selection(
        &mut self,
        action: RowAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.selected_task.clone() else {
            self.report("select a task first", cx);
            return;
        };
        self.perform_row_action(action, id, window, cx);
    }

    /// Say something in the sidebar banner. Same slot the server's own errors
    /// use, and cleared by the next successful refresh.
    pub(crate) fn report(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.app_state
            .update(cx, |state, cx| state.report(message, cx));
    }

    fn latest_spec_id(&self, id: &TaskId, cx: &App) -> Option<SpecId> {
        self.app_state
            .read(cx)
            .latest_spec(id)
            .map(|spec| spec.id.clone())
    }

    fn github_url(&self, id: &TaskId, cx: &App) -> Option<String> {
        let state = self.app_state.read(cx);
        state.github_url(state.task(id)?)
    }

    pub(crate) fn clear_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.bundle_delete_armed = None;
        // Escape and the task view's ✕ mean "put this away" — the middle
        // column returns to the catalog. A real navigation, so ⌘[ can change
        // your mind about it.
        self.navigate(MiddleView::AllTasks, cx);
        // The view this just left may have held the focused element — the
        // review composer `begin_review` focuses. Hand focus back to the root:
        // this is a dismissal, so the caret should not land back in a composer
        // the human just escaped out of. `on_focus_lost` would catch it a
        // frame later; doing it here means there is no frame in between with
        // no keyboard.
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// Cmd-n: open the issue composer window, or re-front it if it's
    /// already up (one drafting surface, like Zed's quick capture — not a
    /// window per draft).
    fn open_issue_window(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = self.issue_window {
            if handle
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
            {
                return;
            }
        }
        let app_state = self.app_state.clone();
        let input = self.issue_input.clone();
        let workspace = cx.entity().downgrade();
        // Opening a window re-enters the platform — deferred off this
        // window's event dispatch, the way Zed opens its windows.
        cx.spawn(async move |this, cx| {
            let handle = cx
                .update(|cx| {
                    let options = issue_composer::window_options(cx);
                    cx.open_window(options, |window, cx| {
                        cx.new(|cx| IssueComposer::new(app_state, input, workspace, window, cx))
                    })
                })
                .ok();
            if let Some(handle) = handle {
                this.update(cx, |this: &mut Workspace, _| {
                    this.issue_window = Some(handle)
                })
                .ok();
            }
        })
        .detach();
    }

    /// Cmd-shift-n: open the Add Repo window, or re-front it. One drafting
    /// surface, like [`Self::open_issue_window`].
    fn open_repo_window(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = self.repo_window {
            if handle
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
            {
                return;
            }
        }
        let app_state = self.app_state.clone();
        let input = self.repo_input.clone();
        let workspace = cx.entity().downgrade();
        cx.spawn(async move |this, cx| {
            let handle = cx
                .update(|cx| {
                    let options = repo_composer::window_options(cx);
                    cx.open_window(options, |window, cx| {
                        cx.new(|cx| RepoComposer::new(app_state, input, workspace, window, cx))
                    })
                })
                .ok();
            if let Some(handle) = handle {
                this.update(cx, |this: &mut Workspace, _| {
                    this.repo_window = Some(handle)
                })
                .ok();
            }
        })
        .detach();
    }

    /// Track a repository, and select it once a snapshot holds it.
    ///
    /// The intent is parked on the slug rather than an id because this client
    /// applies snapshots, never responses — there is no id to select by until
    /// the refresh that follows the POST lands.
    pub(crate) fn add_repo(
        &mut self,
        owner: String,
        name: String,
        slug: String,
        cx: &mut Context<Self>,
    ) {
        self.pending_repo_selection = Some(slug);
        self.app_state
            .update(cx, |state, cx| state.create_project(owner, name, cx));
        cx.notify();
    }

    /// Point the window at one repo, or at all of them.
    ///
    /// Clears an inspector selection belonging to a *different* repo: leaving
    /// it would sit the right sidebar open on a task the window is no longer
    /// showing.
    pub(crate) fn select_project(
        &mut self,
        filter: ProjectFilter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.project_filter = filter;
        self.pending_repo_selection = None;
        let stale = {
            let state = self.app_state.read(cx);
            self.selected_task
                .as_ref()
                .and_then(|id| state.task(id))
                .is_some_and(|task| !self.project_filter.admits(&task.project_id))
        };
        if stale {
            self.clear_selection(window, cx);
        }
        cx.notify();
    }

    /// Pause, archive or reactivate a repo.
    ///
    /// The selection is deliberately left alone, including when the repo being
    /// archived is the one on screen: you archive a repo while looking at it,
    /// and jumping to All repos in the same click hides the thing you were
    /// about to check.
    fn set_project_status(&mut self, id: ProjectId, status: ProjectStatus, cx: &mut Context<Self>) {
        self.app_state
            .update(cx, |state, cx| state.set_project_status(id, status, cx));
    }

    fn close_switcher(&mut self, cx: &mut Context<Self>) {
        self.project_switcher.update(cx, |popover, cx| {
            popover.close(cx);
        });
    }

    /// Adopt a just-added repo once a snapshot names it. Called from the
    /// render pass, which is the first place that can know the id exists.
    fn settle_pending_repo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(slug) = self.pending_repo_selection.clone() else {
            return;
        };
        let id = self
            .app_state
            .read(cx)
            .projects
            .iter()
            .find(|project| project.slug().eq_ignore_ascii_case(&slug))
            .map(|project| project.id.clone());
        if let Some(id) = id {
            self.pending_repo_selection = None;
            self.select_project(ProjectFilter::One(id), window, cx);
        }
    }

    // Chrome

    /// A title-bar icon button at the design spec's metrics: 14px icon with
    /// 8px horizontal / 7px vertical padding, so the button fills the bar.
    fn title_bar_button(
        id: &'static str,
        icon: gpui::Svg,
    ) -> gpuikit::elements::icon_button::IconButton {
        icon_button(id, icon)
            .width(px(30.))
            .height(px(28.))
            .icon_size(px(14.))
    }

    /// The switcher's trigger: what the window is looking at, and a chevron
    /// saying it can be changed.
    ///
    /// Nothing renders before the first snapshot — a placeholder would be a
    /// claim about a repo we have not read — and with one repo configured it
    /// is that repo's slug, so a single-repo window reads exactly as it did
    /// before there was a switcher.
    fn render_switcher_trigger(
        workspace: &WeakEntity<Self>,
        _window: &mut Window,
        cx: &mut App,
    ) -> gpui::AnyElement {
        let label = workspace
            .read_with(cx, |this, cx| {
                let state = this.app_state.read(cx);
                projects::switcher_label(&state.projects, &this.project_filter)
            })
            .ok()
            .flatten();
        let theme = cx.theme().clone();
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.))
            .pl(px(6.))
            .pr(px(4.))
            .py(px(2.))
            .rounded(px(4.))
            .text_sm()
            .text_color(theme.fg_muted())
            .when(label.is_some(), |el| {
                let hover_bg = theme.surface_secondary();
                el.hover(move |el| el.bg(hover_bg))
            })
            .children(label.clone())
            .when(label.is_some(), |el| {
                el.child(
                    Icons::chevron_down()
                        .size(px(10.))
                        .text_color(theme.fg_muted()),
                )
            })
            .into_any_element()
    }

    /// The switcher's rows: All repos, then every project in
    /// [`projects::switcher_order`], then Add Repo.
    fn render_switcher_content(
        workspace: &WeakEntity<Self>,
        _window: &mut Window,
        cx: &mut App,
    ) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let Ok((rows, filter, several)) = workspace.read_with(cx, |this, cx| {
            let state = this.app_state.read(cx);
            let rows: Vec<_> = projects::switcher_order(&state.projects)
                .into_iter()
                .map(|project| {
                    (
                        project.id.clone(),
                        project.slug(),
                        projects::status_note(project.status),
                        projects::status_actions(project.status),
                    )
                })
                .collect();
            let several = state.projects.len() > 1;
            (rows, this.project_filter.clone(), several)
        }) else {
            return div().into_any_element();
        };

        let row_style = |el: Stateful<Div>, selected: bool| {
            let hover_bg = theme.surface_secondary();
            el.flex()
                .flex_col()
                .gap(px(1.))
                .px(px(10.))
                .py(px(5.))
                .rounded(px(4.))
                .cursor_pointer()
                .when(selected, |el| el.bg(theme.surface_tertiary()))
                .hover(move |el| el.bg(hover_bg))
        };

        let mut list = div()
            .flex()
            .flex_col()
            .gap(px(1.))
            .p(px(4.))
            .min_w(px(240.))
            .text_sm()
            .text_color(theme.fg());

        // Only offered when there is something to be "all" of. With one repo
        // configured the window is already showing all of it.
        if several {
            let selected = filter.selected().is_none();
            list = list.child(
                row_style(div().id("switcher-all"), selected)
                    .on_click({
                        let workspace = workspace.clone();
                        move |_event, window, cx| {
                            workspace
                                .update(cx, |this, cx| {
                                    this.select_project(ProjectFilter::All, window, cx);
                                    this.close_switcher(cx);
                                })
                                .ok();
                        }
                    })
                    .child("All repos"),
            );
        }

        for (id, slug, note, actions) in rows {
            let selected = filter.selected() == Some(&id);
            let mut row = row_style(
                div().id(SharedString::from(format!("switcher-{id}"))),
                selected,
            )
            .on_click({
                let workspace = workspace.clone();
                let id = id.clone();
                move |_event, window, cx| {
                    workspace
                        .update(cx, |this, cx| {
                            this.select_project(ProjectFilter::One(id.clone()), window, cx);
                            this.close_switcher(cx);
                        })
                        .ok();
                }
            })
            .child(div().truncate().child(slug));
            // Only a repo that is subtracting something carries a note; the
            // ordinary case earns no badge.
            if let Some(note) = note {
                row = row.child(div().text_xs().text_color(theme.fg_muted()).child(note));
            }
            // The status verbs sit under the repo they act on rather than in a
            // submenu: there are at most two, and a repo's pipeline stopping is
            // not a thing to bury.
            row = row.child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(8.))
                    .pt(px(2.))
                    .text_xs()
                    .children(actions.into_iter().map(|action| {
                        let workspace = workspace.clone();
                        let id = id.clone();
                        div()
                            .id(SharedString::from(format!(
                                "switcher-{id}-{}",
                                action.status.as_str()
                            )))
                            .text_color(theme.fg_muted())
                            .cursor_pointer()
                            .tooltip(tooltip(action.note))
                            .hover({
                                let fg = theme.fg();
                                move |el| el.text_color(fg)
                            })
                            .on_click(move |_event, _window, cx| {
                                workspace
                                    .update(cx, |this, cx| {
                                        this.set_project_status(id.clone(), action.status, cx);
                                        this.close_switcher(cx);
                                    })
                                    .ok();
                            })
                            .child(action.label)
                    })),
            );
            list = list.child(row);
        }

        list.child(
            div()
                .mt(px(2.))
                .pt(px(4.))
                .border_t_1()
                .border_color(theme.border_subtle())
                .child(
                    row_style(div().id("switcher-add-repo"), false)
                        .on_click({
                            let workspace = workspace.clone();
                            move |_event, _window, cx| {
                                workspace
                                    .update(cx, |this, cx| {
                                        this.close_switcher(cx);
                                        this.open_repo_window(cx);
                                    })
                                    .ok();
                            }
                        })
                        .child("Add repo…"),
                ),
        )
        .into_any_element()
    }

    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mode = self.app_state.read(cx).mode;

        title_bar()
            .child_left(
                Self::title_bar_button("toggle-left-sidebar", Icons::panel_left())
                    .selected(self.left_sidebar.is_open())
                    .tooltip(tooltip("Toggle sidebar (⌘B)"))
                    .on_click(|_event, window, cx| {
                        window.dispatch_action(Box::new(ToggleLeftDock), cx);
                    }),
            )
            // The repo the working set belongs to, and the control that
            // changes it.
            .child_left(self.project_switcher.clone())
            // The middle column's history. Greyed rather than absent at the
            // ends of the stack, so the affordance has a fixed address.
            .child_left(
                Self::title_bar_button("history-back", Icons::arrow_left())
                    .disabled(!self.nav.can_go_back())
                    .tooltip(tooltip("Back (⌘[)"))
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.history_back(cx);
                    })),
            )
            .child_left(
                Self::title_bar_button("history-forward", Icons::arrow_right())
                    .disabled(!self.nav.can_go_forward())
                    .tooltip(tooltip("Forward (⌘])"))
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.history_forward(cx);
                    })),
            )
            .child_right(
                Self::title_bar_button("mode-play", Icons::play())
                    .selected(mode == Some(Mode::Play))
                    .tooltip(tooltip("Play — work moves on its own"))
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.set_mode(Mode::Play, cx);
                    })),
            )
            .child_right(
                Self::title_bar_button("mode-pause", Icons::pause())
                    .selected(mode == Some(Mode::Pause))
                    .tooltip(tooltip("Pause — no new work starts"))
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.set_mode(Mode::Pause, cx);
                    })),
            )
            .child_right(
                Self::title_bar_button("refresh", Icons::reload())
                    .tooltip(tooltip("Refresh"))
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.app_state.update(cx, |state, cx| state.refresh(cx));
                    })),
            )
            .child_right(
                Self::title_bar_button("toggle-right-sidebar", Icons::panel_right())
                    .selected(self.right_sidebar.is_open())
                    .tooltip(tooltip("Toggle chat (⌘R)"))
                    .on_click(|_event, window, cx| {
                        window.dispatch_action(Box::new(ToggleRightDock), cx);
                    }),
            )
    }

    /// The right pane is the orchestrator chat — always on screen (⌘R hides
    /// it), per the v3 design. The inspector it used to hold moved into the
    /// middle column's tabs.
    fn render_right_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let chat = self.render_chat(cx).into_any_element();
        sidebar(SidebarSide::Right, self.right_sidebar.width)
            .on_resize_start({
                let entity = cx.entity().downgrade();
                move |_event, _window, cx| {
                    if let Some(workspace) = entity.upgrade() {
                        workspace.update(cx, |this, cx| {
                            this.resizing = Some(SidebarSide::Right);
                            cx.notify();
                        });
                    }
                }
            })
            .child(div().flex().flex_col().flex_1().min_h(px(0.)).child(chat))
    }

    /// One chat row. Durable turns and the live tick's trail entries share
    /// one flat list, so a turn that went text → tool → text reads down the
    /// page in the order it happened instead of overwriting itself.
    fn render_chat_row(&self, ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        // Owned projection first — the markdown cache needs `cx` mutably
        // after the state borrow ends.
        let view = {
            let state = self.app_state.read(cx);
            match state.chat_row(ix) {
                None => ChatRowView::Empty,
                // `.get`, not indexing: a list can be asked for a row the
                // state has moved past, and a blank row beats a panic inside
                // a list item.
                Some(row) => match (row.key, row.kind) {
                    (_, ChatRowKind::Message(index)) => {
                        match state.orchestrator_messages.get(index) {
                            Some(message) => ChatRowView::Message {
                                seq: message.seq,
                                role: message.role,
                                created_at: message.created_at,
                                content: message.content.clone(),
                            },
                            None => ChatRowView::Empty,
                        }
                    }
                    (key, ChatRowKind::Text(text)) => ChatRowView::Text {
                        key: key.markdown_key(),
                        text: text.to_string(),
                        live: row.live_tail,
                    },
                    (ChatRowKey::Entry(id), ChatRowKind::Tools(labels)) => ChatRowView::Tools {
                        id,
                        labels: labels.to_vec(),
                    },
                    // Unreachable: only entries carry tool groups.
                    (ChatRowKey::Message(_), ChatRowKind::Tools(_)) => ChatRowView::Empty,
                },
            }
        };

        match view {
            ChatRowView::Empty => div().into_any_element(),
            ChatRowView::Text { key, text, live } => self.render_trail_text(key, &text, live, cx),
            ChatRowView::Tools { id, labels } => self.render_tool_group(id, labels, cx),
            ChatRowView::Message {
                seq,
                role,
                created_at,
                content,
            } => self.render_chat_message(seq, role, created_at, content, cx),
        }
    }

    /// The chrome every trail row sits in: reading-width, tighter vertically
    /// than a message so a text/tool/text sequence reads as one turn's work
    /// rather than three messages.
    fn trail_row(body: impl IntoElement) -> Div {
        div().w_full().px(px(12.)).py(px(3.)).child(
            div()
                .max_w(CHAT_MAX_WIDTH)
                .w_full()
                .mx_auto()
                .child(body.into_any_element()),
        )
    }

    /// A text segment of the live trail. The tail is the reply being written,
    /// so it reads at full contrast; anything a tool call has closed off is
    /// working narration, and is muted.
    ///
    /// Partial markdown is safe here: gpuikit parses with `pulldown-cmark`,
    /// which closes open constructs at end-of-input, so an unterminated fence
    /// renders as the best reading of what has arrived.
    fn render_trail_text(
        &self,
        key: String,
        text: &str,
        live: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let entity = self.markdown.borrow_mut().entity(key, text, cx);
        Self::trail_row(
            div()
                .px(px(2.))
                .text_sm()
                .text_color(if live { theme.fg() } else { theme.fg_muted() })
                .child(markdown_block(&entity, cx)),
        )
        .into_any_element()
    }

    /// A run of consecutive tool calls, as one quiet expandable row: a dozen
    /// curls are one step of the agent's work, not a dozen messages. A single
    /// call has nothing to expand, so it just shows its label.
    fn render_tool_group(
        &self,
        id: ChatEntryId,
        labels: Vec<String>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let expanded = self.expanded_tools.contains(&id);
        let single = labels.len() == 1;
        let marker = if single {
            "·"
        } else if expanded {
            "▾"
        } else {
            "▸"
        };
        let summary = match (single, expanded) {
            (true, _) => labels[0].clone(),
            (false, true) => format!("{} tool calls", labels.len()),
            // Collapsed: the newest label, because that is the one still
            // running or most recently done.
            (false, false) => format!(
                "{} tool calls · {}",
                labels.len(),
                labels.last().cloned().unwrap_or_default()
            ),
        };

        let header = div()
            .flex()
            .flex_row()
            .items_start()
            .gap(px(6.))
            .px(px(2.))
            .text_xs()
            .text_color(theme.fg_muted())
            .child(div().flex_none().w(px(10.)).child(marker))
            // The server already caps a label at 120 chars, but a narrow
            // window is narrower than that.
            .child(div().flex_1().overflow_hidden().truncate().child(summary));

        let mut row = div().flex().flex_col().gap(px(2.)).child(
            div()
                // Keyed on the entry id, not the row index: rows shift when a
                // trail's reply segment retires, and an element id that moves
                // under the pointer drops the hover it was showing.
                .id(("tool-group", id as usize))
                .when(!single, |el| {
                    el.cursor_pointer()
                        .on_click(cx.listener(move |this, _event, _window, cx| {
                            this.toggle_tool_group(id, cx);
                        }))
                })
                .child(header),
        );
        if expanded && !single {
            row = row.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .pl(px(18.))
                    .text_xs()
                    .text_color(theme.fg_muted())
                    .children(
                        labels
                            .into_iter()
                            .map(|label| div().w_full().overflow_hidden().truncate().child(label)),
                    ),
            );
        }
        Self::trail_row(row).into_any_element()
    }

    /// Open or close a tool group. The row changes height, so it is
    /// re-measured — by key, since the index it sits at can move.
    fn toggle_tool_group(&mut self, id: ChatEntryId, cx: &mut Context<Self>) {
        if !self.expanded_tools.remove(&id) {
            self.expanded_tools.insert(id);
        }
        if let Some(ix) = self
            .chat_keys
            .iter()
            .position(|key| *key == ChatRowKey::Entry(id))
        {
            self.chat_list.remeasure_items(ix..ix + 1);
        }
        cx.notify();
    }

    /// One durable conversation turn. User turns render as cards (their text
    /// is verbatim, not markdown); assistant turns render as full-width
    /// markdown on the pane background, Zed's agent-panel layout. Event and
    /// system rows are quiet one-liners. Every substantive row gets a
    /// hover-revealed timestamp + copy affordance.
    fn render_chat_message(
        &self,
        seq: i64,
        role: ChatRole,
        created_at: chrono::DateTime<chrono::Utc>,
        content: String,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme().clone();

        let body: gpui::AnyElement = match role {
            ChatRole::User => div()
                .p(px(10.))
                .rounded(px(8.))
                .bg(theme.surface_secondary())
                .border_1()
                .border_color(theme.border_subtle())
                .text_sm()
                .text_color(theme.fg())
                .child(content.clone())
                .into_any_element(),
            ChatRole::Assistant => {
                let entity = self
                    .markdown
                    .borrow_mut()
                    .entity(format!("chat:{seq}"), &content, cx);
                div()
                    .px(px(2.))
                    .text_sm()
                    .text_color(theme.fg())
                    .child(markdown_block(&entity, cx))
                    .into_any_element()
            }
            ChatRole::Event => div()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(6.))
                .px(px(2.))
                .text_xs()
                .text_color(theme.fg_muted())
                .child(div().flex_none().child("●").opacity(0.5))
                .child(div().flex_1().child(content.clone()))
                .into_any_element(),
            // A session seam. The conversation reads as continuous here but
            // the orchestrator's memory does not, so it renders as a divider
            // rather than sitting in the flow of turns.
            ChatRole::System => div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .text_xs()
                .text_color(theme.fg_muted())
                .child(div().flex_1().h(px(1.)).bg(theme.border_subtle()))
                .child(div().flex_none().child(content.clone()))
                .child(div().flex_1().h(px(1.)).bg(theme.border_subtle()))
                .into_any_element(),
        };

        // Timestamp + copy, floated over the row's top-right corner on
        // hover only — out of flow, so revealing them can't shift layout.
        let affordances = matches!(role, ChatRole::User | ChatRole::Assistant).then(|| {
            div()
                .absolute()
                .top(px(-6.))
                .right(px(4.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.))
                .px(px(6.))
                .py(px(2.))
                .rounded(px(5.))
                .bg(theme.surface())
                .border_1()
                .border_color(theme.border_subtle())
                .opacity(0.)
                .group_hover("chat-msg", |el| el.opacity(1.))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.fg_muted())
                        .child(time::relative(created_at)),
                )
                .child(
                    // Keyed on the seq, not the row index: rows shift when a
                    // trail's reply segment retires, and an element id that
                    // moves under the pointer drops the hover it was showing.
                    icon_button(("copy-msg", seq as usize), Icons::copy())
                        .width(px(22.))
                        .height(px(20.))
                        .icon_size(px(12.))
                        .tooltip(tooltip("Copy message"))
                        .on_click(move |_event, _window, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(content.clone()));
                        }),
                )
        });

        div()
            .w_full()
            .px(px(12.))
            .py(px(6.))
            .child(
                div()
                    .max_w(CHAT_MAX_WIDTH)
                    .w_full()
                    .mx_auto()
                    .relative()
                    .group("chat-msg")
                    .child(body)
                    .children(affordances),
            )
            .into_any_element()
    }

    /// Bring the virtualized list in line with the chat's row keys, once per
    /// frame.
    ///
    /// A longest-common-prefix diff plus one `splice` covers every way the
    /// rows can move: appending a turn, inserting one *above* an open trail,
    /// the trail's reply segment retiring when the durable reply lands, and a
    /// server whose seqs started over. Everything above the change point
    /// keeps its measurements and the scroll position with them.
    ///
    /// When nothing structural moved but the tick did, the last row is
    /// re-measured instead: `splice` would also re-measure, but it resets the
    /// offset *within* the item, which makes streaming text stutter.
    fn sync_chat_list(&mut self, cx: &mut Context<Self>) {
        let (keys, tick_revision) = {
            let state = self.app_state.read(cx);
            (state.chat_row_keys(), state.tick_revision)
        };

        let prefix = self
            .chat_keys
            .iter()
            .zip(keys.iter())
            .take_while(|(old, new)| old == new)
            .count();
        if prefix < self.chat_keys.len() || prefix < keys.len() {
            self.chat_list
                .splice(prefix..self.chat_keys.len(), keys.len() - prefix);
            // Evict the parses of rows that went away. Without this the
            // shared cache grows by one orphaned entry per turn, forever.
            let live: HashSet<ChatRowKey> = keys.iter().copied().collect();
            let mut markdown = self.markdown.borrow_mut();
            for key in &self.chat_keys {
                if !live.contains(key) {
                    markdown.remove(key.markdown_key());
                }
            }
            self.chat_keys = keys;
        } else if tick_revision != self.chat_tick_revision && !self.chat_keys.is_empty() {
            let last = self.chat_keys.len() - 1;
            self.chat_list.remeasure_items(last..last + 1);
        }
        self.chat_tick_revision = tick_revision;
    }

    fn render_chat(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        // Zed's agent-panel pattern: a virtualized `list` pinned to the
        // conversation tail; rows are synced in `Render::render` by
        // `sync_chat_list`. Item rendering goes back through the workspace
        // (via `processor`) for the markdown cache.
        let messages = list(
            self.chat_list.clone(),
            cx.processor(move |this, ix: usize, _window, cx| this.render_chat_row(ix, cx)),
        );

        // The footer's status line. It leads with an elapsed clock because a
        // clock reads as working and a spinner reads as hung — and because
        // the clock is correct through extended thinking and slow tool calls,
        // when no text is arriving at all. It lives here rather than in the
        // list for two reasons: it must stay visible when the human has
        // scrolled up, and a value that changes once a second has no business
        // re-measuring a list item every second.
        let status = {
            let state = self.app_state.read(cx);
            match &state.orchestrator_tick {
                Some(tick) => Some(format!("{} · working…", time::elapsed(tick.started_at))),
                // The orchestrator owes a reply whenever the newest turn is
                // input (a user message or a pipeline event) — same
                // definition the server's tick loop uses. This covers the
                // gap the tick can't: input landed, no tick has announced
                // itself yet.
                None => state
                    .orchestrator_messages
                    .last()
                    .is_some_and(|message| message.role.is_input())
                    .then(|| "Thinking".to_string()),
            }
        };

        // The tail pin has released (user scrolled up) — offer the way back.
        let show_jump_to_newest = !self.chat_keys.is_empty() && !self.chat_list.is_following_tail();

        let composer = self.render_chat_composer(cx);

        div()
            .flex()
            .flex_col()
            .size_full()
            .map(|el| {
                if self.chat_keys.is_empty() {
                    el.child(self.render_chat_empty_state(cx))
                } else {
                    el.child(
                        div()
                            .relative()
                            .flex_1()
                            .min_h(px(0.))
                            .w_full()
                            .child(messages.size_full().py(px(8.)))
                            .when(show_jump_to_newest, |el| {
                                el.child(
                                    div().absolute().bottom(px(10.)).right(px(16.)).child(
                                        icon_button("jump-to-newest", Icons::pin_bottom())
                                            .width(px(28.))
                                            .height(px(28.))
                                            .icon_size(px(14.))
                                            .tooltip(tooltip("Jump to newest"))
                                            .on_click(cx.listener(|this, _event, _window, cx| {
                                                this.chat_list.scroll_to_end();
                                                this.chat_list.set_follow_mode(FollowMode::Tail);
                                                cx.notify();
                                            })),
                                    ),
                                )
                            }),
                    )
                }
            })
            // Fixed footer, above the composer and outside the scrolling
            // list: the human who has scrolled up still needs to know
            // whether the orchestrator is working.
            .when_some(status, |el, status| {
                el.child(
                    div().flex_none().w_full().px(px(12.)).pb(px(4.)).child(
                        div()
                            .max_w(CHAT_MAX_WIDTH)
                            .w_full()
                            .mx_auto()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.))
                            .text_xs()
                            .text_color(theme.fg_muted())
                            .child(loading_indicator().ellipsis().xsmall())
                            .child(status),
                    ),
                )
            })
            .child(composer)
    }

    /// The chat composer: grows with its draft (three lines minimum, ten
    /// maximum), sends on ⌘↩, and gates the send button on content the way
    /// the review form does.
    fn render_chat_composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let draft = self.input.read(cx).content();
        let has_text = !draft.trim().is_empty();
        let lines = draft.lines().count().clamp(3, 10);
        let composer_height = px(22. * lines as f32 + 20.);

        div()
            .flex_none()
            .p(px(8.))
            .border_t_1()
            .border_color(theme.border_subtle())
            .flex()
            .flex_row()
            .items_end()
            .gap(px(8.))
            .text_sm()
            .child(
                // The multiline input fills its parent, so the parent
                // must own a height — unsized, it collapses to zero.
                div()
                    .flex_1()
                    .h(composer_height)
                    .child(text_area(&self.input, cx).size_full()),
            )
            .child(
                div()
                    .id("chat-send")
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .px(px(10.))
                    .py(px(4.))
                    .rounded(px(5.))
                    .border_1()
                    .border_color(theme.border_secondary())
                    .text_xs()
                    .map(|el| {
                        if has_text {
                            el.cursor_pointer()
                                .text_color(theme.fg())
                                .hover({
                                    let hover_bg = theme.surface_secondary();
                                    move |el| el.bg(hover_bg)
                                })
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.send_chat(cx);
                                }))
                        } else {
                            el.text_color(theme.fg_muted()).opacity(0.5)
                        }
                    })
                    .child("Send")
                    .child(kbd("⌘↩")),
            )
    }

    /// What an empty conversation says about itself.
    fn render_chat_empty_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(8.))
            .p(px(16.))
            .child(
                div()
                    .text_sm()
                    .text_color(theme.fg())
                    .child("Talk to the orchestrator"),
            )
            .child(
                div()
                    .max_w(px(420.))
                    .text_center()
                    .text_xs()
                    .text_color(theme.fg_muted())
                    .child(
                        "It can queue and prioritize work, answer questions about \
                         tasks and specs, and file issues. Press ⌘↩ to send.",
                    ),
            )
    }

    fn render_center(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let loaded = self.app_state.read(cx).loaded;

        let pane = div()
            .flex()
            .flex_col()
            .flex_grow(1.)
            .h_full()
            .overflow_hidden()
            .bg(theme.bg());

        if !loaded {
            return pane.child(
                div()
                    .p(px(16.))
                    .text_sm()
                    .text_color(theme.fg_muted())
                    .child("Connecting to the tasks server…"),
            );
        }

        // The body must be a shrinkable flex child (`flex_1` + `min_h(0)`),
        // never `size_full`: 100% of the pane plus anything above it
        // overflows the clip and cuts off the bottom.
        let body = match self.nav.current().clone() {
            MiddleView::AllTasks => self.render_tasks(cx).into_any_element(),
            MiddleView::Task(id) => self.render_task_pane(&id, cx),
        };
        pane.child(div().flex_1().min_h(px(0.)).overflow_hidden().child(body))
    }

    /// A selected task's tab set: the strip, then the active tab's body.
    fn render_task_pane(&self, id: &TaskId, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        // A task can leave the working set while history still points at it —
        // a retired row is an honest sentence, not a stale render.
        if self.app_state.read(cx).task(id).is_none() {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .p(px(16.))
                .text_sm()
                .text_color(theme.fg_muted())
                .child("That task is no longer in the working set.")
                .into_any_element();
        }

        let body: gpui::AnyElement = match self.task_tab {
            // The landing tab. For now this is the inspector's content
            // wholesale — milestone 3 redistributes it across Overview and
            // Brief and adds the orchestrator-context block.
            TaskTab::Overview => div()
                .id("task-overview-scroll")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .child(self.render_inspector(cx))
                .into_any_element(),
            other => self.render_tab_placeholder(other, cx),
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .child(self.render_tab_bar(cx))
            .child(body)
            .into_any_element()
    }

    /// The tab strip, hand-rolled in Zed's idiom: a quiet bar with a bottom
    /// rule; the active tab reads at full contrast with an accent underline,
    /// inactive tabs are muted and warm on hover. Stateless over
    /// [`Self::task_tab`] — the strip is a projection, not an entity.
    fn render_tab_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        div()
            .flex_none()
            .flex()
            .flex_row()
            .items_end()
            .gap(px(2.))
            .px(px(8.))
            .border_b_1()
            .border_color(theme.border_subtle())
            .children(TaskTab::ALL.into_iter().map(|tab| {
                let active = tab == self.task_tab;
                div()
                    .id(tab.id())
                    .px(px(10.))
                    .py(px(6.))
                    .mb(px(-1.))
                    .border_b_2()
                    .border_color(if active {
                        theme.accent()
                    } else {
                        gpui::Hsla::transparent_black()
                    })
                    .text_sm()
                    .text_color(if active { theme.fg() } else { theme.fg_muted() })
                    .cursor_pointer()
                    .when(!active, |el| {
                        let fg = theme.fg();
                        el.hover(move |el| el.text_color(fg))
                    })
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.task_tab = tab;
                        cx.notify();
                    }))
                    .child(tab.label())
            }))
    }

    /// What an unbuilt tab says about itself. Honest and specific: these land
    /// in later milestones of the v3 plan, and a blank pane would read as a
    /// bug rather than a boundary.
    fn render_tab_placeholder(&self, tab: TaskTab, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let (title, detail) = match tab {
            TaskTab::Brief => (
                "Brief",
                "The spec and its review form land here (milestone 3).",
            ),
            TaskTab::AgentFeed => (
                "Agent Feed",
                "The live Claude Code stream from this task's VM lands here (milestone 4).",
            ),
            TaskTab::Changes => (
                "Changes",
                "The build's branch, summary and verification land here (milestone 5).",
            ),
            TaskTab::Overview => ("Overview", ""),
        };
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(6.))
            .p(px(16.))
            .child(div().text_sm().text_color(theme.fg()).child(title))
            .child(
                div()
                    .max_w(px(420.))
                    .text_center()
                    .text_xs()
                    .text_color(theme.fg_muted())
                    .child(detail),
            )
            .into_any_element()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport_width = window.viewport_size().width;

        // Re-clamp on every frame so a shrunk window can't leave a sidebar
        // owning more than its share.
        let left_width = self.left_sidebar.width;
        self.left_sidebar.set_width(left_width, viewport_width);
        let right_width = self.right_sidebar.width;
        self.right_sidebar.set_width(right_width, viewport_width);

        self.sync_chat_list(cx);
        self.sync_palette(cx);
        // The first frame that can know a just-added repo's id: this client
        // applies snapshots, not responses.
        self.settle_pending_repo(window, cx);

        div()
            // A name rather than an index, per #861: this sits at the root of
            // every descendant's id path. It is not decoration either — gpui
            // logs `note_focus_without_node` for a focusable element with no
            // element id, and a node reaches the a11y tree only with both an
            // id and a non-`None` role, so without these three a screen reader
            // announces the whole window in place of the focused workspace.
            .id("workspace")
            .role(gpui::Role::Group)
            .aria_label("Workspace")
            .key_context(WORKSPACE_CONTEXT)
            // The context above is only real because of this: gpui falls back
            // to the root dispatch node when the focused handle is absent from
            // the rendered frame, and that node carries no context of its own.
            // It also registers a bubble-phase mouse-down that focuses the
            // workspace, which is what makes clicking the background *restore*
            // the shortcuts rather than leave them with nowhere to dispatch.
            .track_focus(&self.focus_handle)
            // The containing block the palette overlay positions against.
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .font_family(FONT)
            // Themed default so nothing bottoms out at gpui's black.
            .text_color(cx.theme().fg())
            .on_action(cx.listener(|this, _: &ToggleLeftDock, _window, cx| {
                this.toggle_sidebar(SidebarSide::Left, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleRightDock, _window, cx| {
                this.toggle_sidebar(SidebarSide::Right, cx);
            }))
            .on_action(cx.listener(|this, _: &NewIssue, _window, cx| {
                this.open_issue_window(cx);
            }))
            .on_action(cx.listener(|this, _: &AddRepo, _window, cx| {
                this.open_repo_window(cx);
            }))
            // Layered dismissal: escape in a focused input blurs it (the
            // input's own binding); the next escape lands here and puts the
            // inspector away.
            //
            // An open row menu takes the first escape, and takes it by
            // *stepping aside*: gpui dispatches key bindings before key-down
            // listeners, so this handler runs before the menu's own escape
            // handler ever sees the keystroke. Clearing the selection here
            // would throw away the row the menu is about and leave the menu
            // itself up, since a handled action stops the event before the
            // listener phase. Propagating instead lets the menu close itself.
            // The two palettes. Both toggle: the same keystroke that opened
            // one closes it, and the other keystroke switches between them
            // without a round trip through the workspace.
            .on_action(cx.listener(|this, _: &ShowCommandPalette, window, cx| {
                this.toggle_palette(PaletteKind::Commands, window, cx);
            }))
            .on_action(cx.listener(|this, _: &GoToAnything, window, cx| {
                this.toggle_palette(PaletteKind::Navigate, window, cx);
            }))
            // Bound in `"Palette > Input"`, so they only ever arrive with the
            // palette's query field focused — but handled here, because the
            // panel is drawn by this view and the selection lives on it.
            .on_action(cx.listener(|this, _: &SelectNextRow, _window, cx| {
                this.move_palette_selection(1, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectPrevRow, _window, cx| {
                this.move_palette_selection(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &Dismiss, window, cx| {
                if this.row_menu_open {
                    this.row_menu_open = false;
                    cx.propagate();
                    return;
                }
                // Selection only — escape must not take the chat pane away;
                // that pane is ⌘R's business.
                if this.selected_task.is_some() {
                    this.clear_selection(window, cx);
                }
            }))
            // The Task menu's verbs, on the selected row.
            .on_action(cx.listener(|this, _: &QueueSelectedTask, window, cx| {
                this.run_on_selection(RowAction::Queue, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ScoutSelectedTask, window, cx| {
                this.run_on_selection(RowAction::ScoutNow, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ApproveSelectedSpec, window, cx| {
                this.run_on_selection(RowAction::ApproveSpec, window, cx);
            }))
            .on_action(cx.listener(|this, _: &HistoryBack, _window, cx| {
                this.history_back(cx);
            }))
            .on_action(cx.listener(|this, _: &HistoryForward, _window, cx| {
                this.history_forward(cx);
            }))
            // A view filter over this window's Tasks list, so it is the
            // workspace's to handle and greys out with no workspace focused.
            .on_action(cx.listener(|this, _: &ToggleShowDone, _window, cx| {
                this.toggle_show_done(cx);
            }))
            // Element-level, like the dock toggles: the pipeline is the
            // workspace's business, so these grey out with no workspace
            // focused rather than acting from the About or Server window.
            // (The Server window has its own pipeline row for that case.)
            .on_action(cx.listener(|this, _: &SetModePlay, _window, cx| {
                this.set_mode(Mode::Play, cx);
            }))
            .on_action(cx.listener(|this, _: &SetModePause, _window, cx| {
                this.set_mode(Mode::Pause, cx);
            }))
            .on_action(cx.listener(|this, _: &SetModeStop, _window, cx| {
                this.set_mode(Mode::Stop, cx);
            }))
            // Drag-resize tracking: the handle only starts the drag; from
            // then on the pointer outruns it, so movement is tracked here at
            // the workspace root (which spans the window).
            .when(self.resizing.is_some(), |el| {
                el.cursor_col_resize().on_mouse_move(cx.listener(
                    move |this, event: &gpui::MouseMoveEvent, _window, cx| {
                        if let Some(side) = this.resizing {
                            let width = match side {
                                SidebarSide::Left => event.position.x,
                                SidebarSide::Right => viewport_width - event.position.x,
                            };
                            this.sidebar_mut(side).set_width(width, viewport_width);
                            cx.notify();
                        }
                    },
                ))
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _event, _window, cx| {
                    if this.resizing.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            // Any press outside an open row menu closes it — clicking away,
            // or right-clicking a different row. Capture phase, so it runs
            // before the row that is about to open the *next* menu sets the
            // flag again; the popup occludes the pointer, so a press on the
            // menu itself never reaches here (choosing an item clears it on
            // its own way through `perform_row_action`).
            .capture_any_mouse_down(cx.listener(|this, _event, _window, _cx| {
                this.row_menu_open = false;
            }))
            .child(self.render_title_bar(cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_grow(1.)
                    .overflow_hidden()
                    .when(self.left_sidebar.is_open(), |el| {
                        el.child(self.render_left_sidebar(cx))
                    })
                    .child(self.render_center(cx))
                    .when(self.right_sidebar.is_open(), |el| {
                        el.child(self.render_right_sidebar(cx))
                    }),
            )
            // Last, so it paints above both sidebars. Two siblings, not a
            // parent and a child: gpui delivers a click to the topmost element
            // and bubbles it through *ancestors*, so a backdrop wrapping the
            // panel would never see a click on the rows underneath it — and
            // would swallow every click on the panel itself.
            .children(self.render_palette(cx).into_iter().flatten())
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
