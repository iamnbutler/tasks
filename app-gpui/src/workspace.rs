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
    actions, div, list, px, App, ClipboardItem, Context, Div, Entity, Focusable, FollowMode,
    ListAlignment, ListState, MouseButton, Window, WindowHandle,
};
use gpuikit::elements::context_menu::{menu_item, MenuItems};
use gpuikit::elements::icon_button::icon_button;
use gpuikit::elements::input::text_area;
use gpuikit::elements::kbd::kbd;
use gpuikit::elements::loading_indicator::loading_indicator;
use gpuikit::elements::tooltip::tooltip;
use gpuikit::input::{InputState, InputStateEvent, SubmitOn};
use gpuikit::theme::{ActiveTheme, Themeable};
use gpuikit::DefaultIcons as Icons;
use tasks_client::api::models::{
    BuildStatus, ChatRole, CloseReason, Mode, SessionStatus, SpecId, SpecQueueStatus, TaskId,
    TaskState,
};

use crate::chat_log::{ChatEntryId, ChatRowKey, ChatRowKind};
use crate::components::{
    markdown_block, sidebar, title_bar, MarkdownCache, SidebarSide, SidebarState,
};
use crate::issue_composer::{self, IssueComposer};
use crate::menus::{self, MenuState};
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
        Dismiss,
        GoToHome,
        GoToTasks,
        GoToQueue,
        GoToActivity,
        GoToChat,
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

#[derive(PartialEq, Clone, Copy)]
pub enum Section {
    Home,
    Tasks,
    Queue,
    Activity,
    Chat,
}

impl Section {
    const ALL: [Section; 5] = [
        Section::Home,
        Section::Tasks,
        Section::Queue,
        Section::Activity,
        Section::Chat,
    ];

    /// The section's name. Rendered exactly once now — in the title bar's
    /// centre. The nav rows are icons and carry it as an accessible name;
    /// `render_center` no longer spends a row on it.
    fn label(self) -> &'static str {
        match self {
            Section::Home => "Home",
            Section::Tasks => "Tasks",
            Section::Queue => "Queue",
            Section::Activity => "Activity",
            Section::Chat => "Chat",
        }
    }

    /// The icon a nav row wears in place of its label.
    fn icon(self) -> gpui::Svg {
        match self {
            Section::Home => Icons::home(),
            Section::Tasks => Icons::list_bullet(),
            Section::Queue => Icons::layers(),
            Section::Activity => Icons::activity_log(),
            Section::Chat => Icons::chat_bubble(),
        }
    }

    /// Element id for the nav row. A name rather than the enumeration index:
    /// these rows have no id'd ancestor, so a bare integer sits at the root
    /// of the id path, which is exactly the collision class #861 is about.
    fn nav_id(self) -> &'static str {
        match self {
            Section::Home => "nav-home",
            Section::Tasks => "nav-tasks",
            Section::Queue => "nav-queue",
            Section::Activity => "nav-activity",
            Section::Chat => "nav-chat",
        }
    }

    /// The ⌘-digit that reaches this section. Bound in `main`; repeated here
    /// only to be *announced* — in the row's tooltip and, via
    /// `aria_keyshortcuts`, to assistive technology.
    fn shortcut(self) -> &'static str {
        match self {
            Section::Home => "⌘1",
            Section::Tasks => "⌘2",
            Section::Queue => "⌘3",
            Section::Activity => "⌘4",
            Section::Chat => "⌘5",
        }
    }
}

pub struct Workspace {
    pub(crate) section: Section,
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
    /// Whether the Tasks list shows its archive of done tasks. Per-window and
    /// resets on relaunch: the app has no settings store, and a view filter
    /// that states its own count in a footer does not need one.
    pub(crate) show_done: bool,
    /// Chat composer.
    pub(crate) input: Entity<InputState>,
    /// Review-form composer in the inspector — feedback for a re-scout or a
    /// question for the orchestrator, depending on which button submits it.
    pub(crate) review_input: Entity<InputState>,
    /// Issue-draft composer shown in the cmd-n window. Owned here, not by
    /// the window, so a dismissed draft survives to the next cmd-n.
    pub(crate) issue_input: Entity<InputState>,
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
        // The composers gate their submit buttons on content, so keystrokes
        // must re-render the workspace, not just the input element.
        cx.observe(&input, |_, _, cx| cx.notify()).detach();
        cx.observe(&review_input, |_, _, cx| cx.notify()).detach();
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
        window.focus(&input.focus_handle(cx), cx);

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
            section: Section::Home,
            left_sidebar: SidebarState::new(true),
            // The inspector is a reading surface (specs, task bodies) —
            // default it wide, like the Swift app's 460pt ideal.
            right_sidebar: SidebarState::new(false).with_width(px(460.)),
            resizing: None,
            app_state,
            server_control,
            selected_task: None,
            show_done: false,
            input,
            review_input,
            issue_input,
            issue_window: None,
            chat_list,
            chat_keys: Vec::new(),
            chat_tick_revision: 0,
            expanded_tools: HashSet::new(),
            markdown: RefCell::new(MarkdownCache::new()),
            row_menu_open: false,
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
        let state = self.sidebar_mut(side);
        state.open = !state.open;
        cx.notify();
    }

    fn go_to_section(&mut self, section: Section, cx: &mut Context<Self>) {
        self.section = section;
        cx.notify();
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
        let busy = self.server_control.read(cx).busy();
        let state = self.app_state.read(cx);
        let menu_state = MenuState {
            serving: state.connected,
            mode: state.mode,
            busy,
            show_done: self.show_done,
        };
        menus::sync(cx, menu_state);
    }

    // --- selection (called from section rows) ---

    pub(crate) fn select_task(&mut self, id: TaskId, cx: &mut Context<Self>) {
        if self.selected_task.as_ref() != Some(&id) {
            // Draft feedback is about one spec — don't carry it to another.
            self.review_input
                .update(cx, |input, cx| input.set_content("", cx));
        }
        self.selected_task = Some(id);
        self.right_sidebar.open = true;
        cx.notify();
    }

    /// Send a message into the orchestrator conversation and jump to Chat so
    /// the reply is visible as it streams in.
    pub(crate) fn ask_orchestrator(&mut self, message: String, cx: &mut Context<Self>) {
        self.app_state
            .update(cx, |state, cx| state.send_orchestrator_message(message, cx));
        self.section = Section::Chat;
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

    /// Show the task's spec in the inspector with the review composer focused
    /// — "Review Spec…" landing where the spec text already is, rather than in
    /// a modal that would cover the thing being judged.
    fn begin_review(&mut self, id: TaskId, window: &mut Window, cx: &mut Context<Self>) {
        self.select_task(id, cx);
        self.right_sidebar.open = true;
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
    fn report(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
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

    pub(crate) fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.selected_task = None;
        self.right_sidebar.open = false;
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

    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // The repo the working set belongs to. A label, not a button: the
        // server offers no way to switch projects yet, and a control that
        // looks live and isn't is worse than a word. Nothing renders before
        // the first snapshot — a placeholder would be a claim about a repo
        // we have not read.
        let (mode, project) = {
            let state = self.app_state.read(cx);
            let project = state
                .projects
                .first()
                .map(|project| format!("{}/{}", project.repo_owner, project.repo_name));
            (state.mode, project)
        };
        let text_muted = cx.theme().fg_muted();

        title_bar()
            .child_left(
                Self::title_bar_button("toggle-left-sidebar", Icons::panel_left())
                    .selected(self.left_sidebar.open)
                    .tooltip(tooltip("Toggle sidebar (⌘B)"))
                    .on_click(|_event, window, cx| {
                        window.dispatch_action(Box::new(ToggleLeftDock), cx);
                    }),
            )
            .child_left(
                div()
                    .pl(px(6.))
                    .text_sm()
                    .text_color(text_muted)
                    .children(project),
            )
            // The section you are looking at, named once. The sidebar's rows
            // are icons and `render_center` draws no header, so this is the
            // only place the word appears.
            .child_center(div().child(self.section.label()))
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
                    .selected(self.right_sidebar.open)
                    .tooltip(tooltip("Toggle inspector (⌘R)"))
                    .on_click(|_event, window, cx| {
                        window.dispatch_action(Box::new(ToggleRightDock), cx);
                    }),
            )
    }

    fn render_left_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (text, text_muted, selected_bg, hover_bg, badge_bg) = (
            theme.fg(),
            theme.fg_muted(),
            theme.surface_tertiary(),
            theme.surface_secondary(),
            theme.surface_tertiary(),
        );
        let active = self.section;

        // Read before the app-state borrow: a run in flight outranks
        // everything the server could tell us, because it is the reason the
        // server stopped telling us anything.
        let running_op = {
            let control = self.server_control.read(cx);
            control
                .run
                .as_ref()
                .filter(|run| run.is_running())
                .map(|run| (run.op, run.started_at))
        };

        let state = self.app_state.read(cx);
        let queued_work = state
            .tasks
            .iter()
            .filter(|task| {
                matches!(
                    task.state,
                    TaskState::Queued
                        | TaskState::Scouting
                        | TaskState::InReview
                        | TaskState::ReadyToBuild
                        | TaskState::Building
                )
            })
            .count();
        // A proactive tick is invisible from every section but Chat, so the
        // Chat row wears the clock. Same slot the Queue count uses.
        let tick_elapsed = state
            .orchestrator_tick
            .as_ref()
            .map(|tick| time::elapsed(tick.started_at));
        // A restart in flight outranks both, and is checked first rather than
        // last: it takes the app's own event stream down, and reporting that
        // drop as a transport error would be the app blaming the server for
        // doing what it was asked. A stale build is usually *why* someone hit
        // restart, so this has to sit above the build warning too.
        //
        // The build warning in turn outranks the error: when this app is
        // older than the server supports, whatever failed underneath is the
        // symptom and "your app is old" is the cause.
        let banner = if let Some((op, started_at)) = running_op {
            Some((
                format!("{}… {}", op.label(), time::elapsed(started_at)),
                false,
            ))
        } else if let Some(warning) = &state.build_warning {
            Some((warning.clone(), true))
        } else if let Some(error) = &state.error {
            Some((error.clone(), true))
        } else if state.loaded && !state.connected {
            Some(("Reconnecting to the tasks server…".to_string(), false))
        } else {
            None
        };

        sidebar(SidebarSide::Left, self.left_sidebar.width)
            .on_resize_start({
                let entity = cx.entity().downgrade();
                move |_event, _window, cx| {
                    if let Some(workspace) = entity.upgrade() {
                        workspace.update(cx, |this, cx| {
                            this.resizing = Some(SidebarSide::Left);
                            cx.notify();
                        });
                    }
                }
            })
            // The rows are icons: the title bar names the section you are in,
            // so spelling it again here would be the second of two. What the
            // word used to do — say which row this is — the tooltip and the
            // accessible name now do. `role` is not decoration: a node reaches
            // the a11y tree only with *both* an id and a non-`None` role, so
            // an `aria_label` on a roleless div is dropped silently.
            .child(div().flex().flex_col().flex_1().pt(px(8.)).children(
                Section::ALL.into_iter().map(|section| {
                    let selected = section == active;
                    let badge = match section {
                        Section::Queue if queued_work > 0 => Some(queued_work.to_string()),
                        Section::Chat => tick_elapsed.clone(),
                        _ => None,
                    };
                    div()
                        .id(section.nav_id())
                        .role(gpui::Role::Tab)
                        .aria_label(section.label())
                        .aria_keyshortcuts(section.shortcut())
                        .tooltip(tooltip(format!(
                            "{} ({})",
                            section.label(),
                            section.shortcut()
                        )))
                        .flex()
                        .flex_row()
                        .items_center()
                        .mx(px(6.))
                        .px(px(10.))
                        .py(px(5.))
                        .rounded(px(5.))
                        .cursor_pointer()
                        .when(!selected, |el| el.hover(move |el| el.bg(hover_bg)))
                        .when(selected, |el| el.bg(selected_bg))
                        .on_click(cx.listener(move |this, _event, _window, cx| {
                            this.go_to_section(section, cx);
                        }))
                        .child(
                            section
                                .icon()
                                .flex_none()
                                .size(px(15.))
                                .text_color(if selected { text } else { text_muted }),
                        )
                        .child(div().flex_1())
                        .when_some(badge, |el, badge| {
                            el.child(
                                div()
                                    .flex_none()
                                    .px(px(6.))
                                    .rounded_full()
                                    .bg(badge_bg)
                                    .text_xs()
                                    .text_color(text_muted)
                                    .child(badge),
                            )
                        })
                }),
            ))
            .child(div().flex_1())
            .when_some(banner, |el, (message, is_error)| {
                el.child(
                    div()
                        .m(px(6.))
                        .p(px(8.))
                        .rounded(px(5.))
                        .bg(cx.theme().surface_secondary())
                        .text_xs()
                        .text_color(if is_error {
                            gpui::hsla(30. / 360., 0.9, 0.6, 1.)
                        } else {
                            cx.theme().fg_muted()
                        })
                        .child(message),
                )
            })
    }

    fn render_right_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let inspector = self.render_inspector(cx);
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
            .child(inspector)
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

        // No header row. Chat always worked this way — "the sidebar already
        // names it" — and now the title bar names every section, so a header
        // here would be the second rendering of a word that should appear
        // once. (If the name belongs back in the pane, this block and the
        // `child_center` in `render_title_bar` are the pair to revert.)
        //
        // The body must be a shrinkable flex child (`flex_1` + `min_h(0)`),
        // never `size_full`: 100% of the pane plus anything above it
        // overflows the clip and cuts off the bottom (chat's composer).
        let body = match self.section {
            Section::Home => self.render_home(cx).into_any_element(),
            Section::Tasks => self.render_tasks(cx).into_any_element(),
            Section::Queue => self.render_queue(cx).into_any_element(),
            Section::Activity => self.render_activity(cx).into_any_element(),
            Section::Chat => self.render_chat(cx).into_any_element(),
        };
        pane.child(div().flex_1().min_h(px(0.)).overflow_hidden().child(body))
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

        div()
            .key_context("Workspace")
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
            .on_action(cx.listener(|this, _: &Dismiss, _window, cx| {
                if this.row_menu_open {
                    this.row_menu_open = false;
                    cx.propagate();
                    return;
                }
                if this.selected_task.is_some() || this.right_sidebar.open {
                    this.clear_selection(cx);
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
            .on_action(cx.listener(|this, _: &GoToHome, _window, cx| {
                this.go_to_section(Section::Home, cx);
            }))
            .on_action(cx.listener(|this, _: &GoToTasks, _window, cx| {
                this.go_to_section(Section::Tasks, cx);
            }))
            .on_action(cx.listener(|this, _: &GoToQueue, _window, cx| {
                this.go_to_section(Section::Queue, cx);
            }))
            .on_action(cx.listener(|this, _: &GoToActivity, _window, cx| {
                this.go_to_section(Section::Activity, cx);
            }))
            .on_action(cx.listener(|this, _: &GoToChat, _window, cx| {
                this.go_to_section(Section::Chat, cx);
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
                    .when(self.left_sidebar.open, |el| {
                        el.child(self.render_left_sidebar(cx))
                    })
                    .child(self.render_center(cx))
                    .when(self.right_sidebar.open, |el| {
                        el.child(self.render_right_sidebar(cx))
                    }),
            )
    }
}
