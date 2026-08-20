//! The action registry: one table, three consumers.
//!
//! [`COMMANDS`] states, for every verb the app offers, what it is called, what
//! action it dispatches, what key equivalent it answers to, which keymap
//! context that binding belongs in, where it sits in the menu bar and when it
//! cannot run. [`bind_keys`] installs the bindings, [`menu_items`] builds the
//! bar out of it, and `crate::palette` lists it. Before this existed those
//! three were three hand-written lists that had to agree, and the way they
//! stopped agreeing was silent: a binding installed after `set_menus` shows no
//! key equivalent, a menu item with no binding shows none either, and nothing
//! warns you in either direction.
//!
//! Two things deliberately stay *outside* the table.
//!
//! - **The handlers.** [`Handler`] records only *which kind* of handler a
//!   command has, because that is what decides which context its binding goes
//!   in. The handlers themselves stay split between `App::on_action`
//!   (`menus::init`) and the workspace root's `.on_action` — and that split is
//!   load-bearing: a menu item greys itself out via `App::is_action_available`,
//!   an action with a global handler is *always* available, so it is exactly
//!   the element-handled ones that correctly grey out while the About or
//!   Server window is focused. Moving the handlers in here would flatten that
//!   distinction into nothing.
//! - **The Edit menu**, which dispatches gpuikit's own input actions through
//!   `MenuItem::os_action`. Those are not this app's actions, they are never
//!   bound here, and they map to AppKit selectors; `menus.rs` writes them out
//!   by hand.
//!
//! Availability is a pure function of [`Facts`], and the [`Selection`] half of
//! it carries a distinction that is the whole design: `Unknown` is the menu
//! bar. The bar cannot grey per selection — `set_menus` leaks a boxed action
//! per item on every rebuild and the selection moves on every arrow key — so
//! selection-dependent verbs read as *available* to it and report their
//! refusal when chosen. The palette passes a real selection and greys
//! honestly. Collapsing the two into `Option<RowContext>` would either grey the
//! Task menu (and rebuild the bar per keystroke) or leave the palette unable to
//! say "no task is selected".

use std::rc::Rc;

use gpui::{Action, App, KeyBinding, KeyBindingContextPredicate, MenuItem};
use tasks_client::api::models::Mode;

use crate::menus::{
    self, About, CloseWindow, Hide, HideOthers, MenuState, Minimize, OpenDataDirectory, Quit,
    RestartServer, RestartServerWhenIdle, RevealServeLog, ShowAll, ShowAutonomyNotice, ShowCharter,
    ShowServerStatus, StopServer, StopServerWhenIdle, Zoom,
};
use crate::palette::{GoToAnything, ShowCommandPalette};
use crate::row_menu::{self, RowAction, RowContext};
use crate::workspace::{
    AddRepo, ApproveSelectedSpec, Dismiss, HistoryBack, HistoryForward, KillAllContainers,
    NewIssue, QueueSelectedTask, ScoutSelectedTask, SetModePause, SetModePlay, SetModeStop,
    ToggleLeftDock, ToggleRightDock, ToggleShowDone,
};

/// The keymap context the workspace root sets on itself. A binding in this
/// context only fires with the workspace focused, which is what a verb acting
/// on the workspace's own state wants.
pub const WORKSPACE_CONTEXT: &str = "Workspace";

/// Which kind of handler a command has, and therefore which keymap context its
/// binding belongs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handler {
    /// `App::on_action`. Acts on whichever window is focused, or on no window
    /// at all — and is therefore always available, in every window.
    Global,
    /// The workspace root's `.on_action`. Greys out with no workspace focused,
    /// which is correct for anything that acts on the selection, the sections
    /// or the docks.
    Workspace,
}

impl Handler {
    /// The keymap context predicate this handler's bindings are installed
    /// under.
    pub const fn context(self) -> Option<&'static str> {
        match self {
            Handler::Global => None,
            Handler::Workspace => Some(WORKSPACE_CONTEXT),
        }
    }
}

/// A top-level menu a command can sit in. Edit is absent on purpose — see the
/// module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// The application menu, named for the app.
    App,
    File,
    View,
    Task,
    Server,
    Window,
}

impl Slot {
    /// The menu's name in the bar. Also the category the palette prefixes a
    /// row with, so "server" as a query finds every server op.
    pub const fn menu_name(self) -> &'static str {
        match self {
            Slot::App => "Tasks",
            Slot::File => "File",
            Slot::View => "View",
            Slot::Task => "Task",
            Slot::Server => "Server",
            Slot::Window => "Window",
        }
    }
}

/// Which row the selection-dependent verbs are being asked about.
///
/// The three cases are not two: see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    /// The asker cannot grey per selection and does not want to try — the menu
    /// bar. Selection-dependent verbs report as available and refuse when
    /// chosen.
    Unknown,
    /// The asker knows the selection, and there isn't one.
    None,
    /// The asker knows the selection, and here is what the row looks like.
    Task(RowContext),
}

/// Everything availability, labelling and checkmarks are derived from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts {
    pub menu: MenuState,
    pub selection: Selection,
}

impl Facts {
    /// The facts as the menu bar sees them: it knows the server's state and
    /// deliberately knows nothing about the selection.
    pub const fn for_menu_bar(menu: MenuState) -> Self {
        Self {
            menu,
            selection: Selection::Unknown,
        }
    }
}

/// One verb, stated once.
pub struct Command {
    /// Stable handle, for tests and element ids. Never rendered.
    pub id: &'static str,
    /// What it is called, when the name does not depend on state.
    pub label: &'static str,
    /// A fresh boxed action. A function pointer rather than a value because
    /// `Box<dyn Action>` is not `Copy` and every consumer needs its own.
    pub action: fn() -> Box<dyn Action>,
    /// The key equivalent, as the keymap spells it.
    pub key: Option<&'static str>,
    pub handler: Handler,
    /// Where it sits in the bar. `None` keeps it out of the bar entirely —
    /// `escape` has no business being an item.
    pub menu: Option<Slot>,
    /// Draw a rule above it.
    pub separator_before: bool,
    /// Why it cannot run right now, given the facts.
    pub refusal: Option<fn(Facts) -> Option<&'static str>>,
    /// Whether it wears a checkmark — a radio group or a view filter whose
    /// position you can read at a glance.
    pub checked: Option<fn(Facts) -> bool>,
    /// A name that depends on state. One item that renames itself is one item;
    /// two items that run the same command are a bug waiting to be reported.
    pub rename: Option<fn(Facts) -> &'static str>,
    /// Offered in the command palette. Off for the two palettes themselves
    /// (you are already in one) and for `escape`, which is not a verb anyone
    /// hunts for by name.
    pub in_palette: bool,
}

impl Command {
    pub const fn new(
        id: &'static str,
        label: &'static str,
        action: fn() -> Box<dyn Action>,
    ) -> Self {
        Self {
            id,
            label,
            action,
            key: None,
            handler: Handler::Global,
            menu: None,
            separator_before: false,
            refusal: None,
            checked: None,
            rename: None,
            in_palette: true,
        }
    }

    pub const fn key(mut self, key: &'static str) -> Self {
        self.key = Some(key);
        self
    }

    /// Element-handled, in the workspace's context.
    pub const fn on_workspace(mut self) -> Self {
        self.handler = Handler::Workspace;
        self
    }

    pub const fn menu(mut self, slot: Slot) -> Self {
        self.menu = Some(slot);
        self
    }

    /// In `slot`, with a rule above it.
    pub const fn separated(mut self, slot: Slot) -> Self {
        self.menu = Some(slot);
        self.separator_before = true;
        self
    }

    pub const fn refusal(mut self, refusal: fn(Facts) -> Option<&'static str>) -> Self {
        self.refusal = Some(refusal);
        self
    }

    pub const fn checked(mut self, checked: fn(Facts) -> bool) -> Self {
        self.checked = Some(checked);
        self
    }

    pub const fn renaming(mut self, rename: fn(Facts) -> &'static str) -> Self {
        self.rename = Some(rename);
        self
    }

    pub const fn out_of_palette(mut self) -> Self {
        self.in_palette = false;
        self
    }

    /// What to call it, given the facts.
    pub fn label(&self, facts: Facts) -> &'static str {
        match self.rename {
            Some(rename) => rename(facts),
            None => self.label,
        }
    }

    /// Why it cannot run, or `None`.
    pub fn refusal_for(&self, facts: Facts) -> Option<&'static str> {
        self.refusal.and_then(|refusal| refusal(facts))
    }

    pub fn is_checked(&self, facts: Facts) -> bool {
        self.checked.is_some_and(|checked| checked(facts))
    }

    /// The key equivalent as macOS writes it (`"⇧⌘S"`), for surfaces that
    /// render their own shortcut text. The menu bar needs none of this — gpui
    /// reads shortcuts out of the keymap while building the bar.
    pub fn rendered_key(&self) -> Option<String> {
        self.key.map(menus::rendered_keystroke)
    }

    /// The row the palette shows: the menu it lives under, then its name. The
    /// category is derived rather than stated, and it is what makes "server"
    /// as a query pull up every server op.
    pub fn palette_label(&self, facts: Facts) -> String {
        match self.menu {
            Some(slot) => format!("{}: {}", slot.menu_name(), self.label(facts)),
            None => self.label(facts).to_string(),
        }
    }
}

/// Every verb the app offers, in menu-bar order within each slot.
pub const COMMANDS: &[Command] = &[
    // --- the application menu ---
    Command::new("about", "About Tasks", || Box::new(About)).menu(Slot::App),
    Command::new("hide", "Hide Tasks", || Box::new(Hide))
        .key("cmd-h")
        .separated(Slot::App),
    Command::new("hide-others", "Hide Others", || Box::new(HideOthers))
        .key("cmd-alt-h")
        .menu(Slot::App),
    Command::new("show-all", "Show All", || Box::new(ShowAll)).menu(Slot::App),
    Command::new("quit", "Quit Tasks", || Box::new(Quit))
        .key("cmd-q")
        .separated(Slot::App),
    // --- File ---
    // The comment that used to sit here said New Issue belongs in this menu
    // once the window that creates one lands. It has landed.
    Command::new("new-issue", "New Issue…", || Box::new(NewIssue))
        .key("cmd-n")
        .on_workspace()
        .menu(Slot::File),
    // The other way to the Add Repo window, for the same reason the palettes
    // are in the bar: a surface reachable only from a popover in the title bar
    // is one most people never find.
    Command::new("add-repo", "Add Repo…", || Box::new(AddRepo))
        .key("cmd-shift-n")
        .on_workspace()
        .menu(Slot::File),
    Command::new("close-window", "Close Window", || Box::new(CloseWindow))
        .key("cmd-w")
        .separated(Slot::File),
    // --- View: middle-column history, browser-style ---
    Command::new("history-back", "Back", || Box::new(HistoryBack))
        .key("cmd-[")
        .on_workspace()
        .menu(Slot::View),
    Command::new("history-forward", "Forward", || Box::new(HistoryForward))
        .key("cmd-]")
        .on_workspace()
        .menu(Slot::View),
    // The two palettes. In the bar because a surface reachable only by
    // knowing its keystroke is a surface most people never find — and out of
    // the palette itself, because you are already in one.
    Command::new("go-to-anything", "Go to Anything…", || {
        Box::new(GoToAnything)
    })
    .key("cmd-p")
    .on_workspace()
    .separated(Slot::View)
    .out_of_palette(),
    Command::new("command-palette", "Command Palette…", || {
        Box::new(ShowCommandPalette)
    })
    .key("shift-cmd-p")
    .on_workspace()
    .menu(Slot::View)
    .out_of_palette(),
    // Below a rule of its own because it filters a section rather than going
    // to one.
    Command::new("toggle-show-done", "Show Done Tasks", || {
        Box::new(ToggleShowDone)
    })
    .key("shift-cmd-d")
    .on_workspace()
    .separated(Slot::View)
    .checked(|facts| facts.menu.show_done),
    // --- Task: the selected row's safe verbs ---
    //
    // Only these three, and only the safe ones. Closing an issue is one click
    // in the row menu and no keystroke anywhere: it is the one verb here that
    // changes something outside this machine.
    Command::new("queue-selected", "Add to Queue", || {
        Box::new(QueueSelectedTask)
    })
    .key(menus::QUEUE_KEYSTROKE)
    .on_workspace()
    .menu(Slot::Task)
    .refusal(|facts| selection_refusal(facts, RowAction::Queue)),
    Command::new("scout-selected", "Scout Now", || {
        Box::new(ScoutSelectedTask)
    })
    .key(menus::SCOUT_KEYSTROKE)
    .on_workspace()
    .menu(Slot::Task)
    .refusal(|facts| selection_refusal(facts, RowAction::ScoutNow)),
    Command::new("approve-selected", "Approve Spec", || {
        Box::new(ApproveSelectedSpec)
    })
    .key(menus::APPROVE_KEYSTROKE)
    .on_workspace()
    .menu(Slot::Task)
    .refusal(|facts| selection_refusal(facts, RowAction::ApproveSpec)),
    // --- Server ---
    //
    // Nothing here is bound, deliberately: a one-keystroke server restart is
    // the foot-gun this menu is trying not to build.
    //
    // Status comes first so you can see what you are about to interrupt.
    Command::new("server-status", "Server Status…", || {
        Box::new(ShowServerStatus)
    })
    .menu(Slot::Server),
    // `tasks reload` with no live pid already *is* a start, so this is one item
    // that renames itself rather than two that run the same command.
    Command::new("restart-server", "Restart Server…", || {
        Box::new(RestartServer)
    })
    .separated(Slot::Server)
    .renaming(|facts| match facts.menu.serving {
        true => "Restart Server…",
        false => "Start Server",
    })
    .refusal(busy_refusal),
    // `--when-idle` and `stop` both need something to be running: with nothing
    // up, the first has nothing to wait for and the second nothing to stop.
    Command::new("restart-server-when-idle", "Restart When Idle…", || {
        Box::new(RestartServerWhenIdle)
    })
    .menu(Slot::Server)
    .refusal(running_server_refusal),
    Command::new("stop-server", "Stop Server…", || Box::new(StopServer))
        .menu(Slot::Server)
        .refusal(running_server_refusal),
    Command::new("stop-server-when-idle", "Stop When Idle…", || {
        Box::new(StopServerWhenIdle)
    })
    .menu(Slot::Server)
    .refusal(running_server_refusal),
    // The pipeline group governs dispatch rather than the process — same menu,
    // different subject, and the prefix is what keeps "Stop Server" and
    // "Pipeline: Stop" from reading as two spellings of one thing.
    Command::new("mode-play", "Pipeline: Play", || Box::new(SetModePlay))
        .on_workspace()
        .separated(Slot::Server)
        .checked(|facts| facts.menu.mode == Some(Mode::Play)),
    Command::new("mode-pause", "Pipeline: Pause", || Box::new(SetModePause))
        .on_workspace()
        .menu(Slot::Server)
        .checked(|facts| facts.menu.mode == Some(Mode::Pause)),
    Command::new("mode-stop", "Pipeline: Stop", || Box::new(SetModeStop))
        .on_workspace()
        .menu(Slot::Server)
        .checked(|facts| facts.menu.mode == Some(Mode::Stop)),
    // In the pipeline group, not the process group: it acts on running work
    // over HTTP (the same durable cancel rows the row menu writes, one per
    // run), never on VMs directly. Queued builds survive it — pause first if
    // the point is that nothing further starts.
    Command::new("kill-all-containers", "Kill All Containers", || {
        Box::new(KillAllContainers)
    })
    .on_workspace()
    .menu(Slot::Server)
    .refusal(running_server_refusal),
    // Under Kill All Containers, in the pipeline group, because both are
    // answers to "make it stop". Neither is `.on_workspace()`: an off switch
    // that greys out because the wrong window is focused is not an off
    // switch, and the Server window is exactly where somebody worried about
    // this already is.
    Command::new("charter", "Charter…", || Box::new(ShowCharter)).menu(Slot::Server),
    Command::new("what-play-does", "What Play Does…", || {
        Box::new(ShowAutonomyNotice)
    })
    .menu(Slot::Server),
    Command::new("reveal-serve-log", "Reveal serve.log", || {
        Box::new(RevealServeLog)
    })
    .separated(Slot::Server),
    Command::new("open-data-directory", "Open Data Directory", || {
        Box::new(OpenDataDirectory)
    })
    .menu(Slot::Server),
    // --- Window ---
    //
    // gpui special-cases the literal menu name "Window" and hands it to AppKit
    // as the windows menu; that is what makes the open-windows list append
    // itself.
    Command::new("minimize", "Minimize", || Box::new(Minimize))
        .key("cmd-m")
        .menu(Slot::Window),
    Command::new("zoom", "Zoom", || Box::new(Zoom)).menu(Slot::Window),
    Command::new("toggle-left-dock", "Toggle Left Dock", || {
        Box::new(ToggleLeftDock)
    })
    .key("cmd-b")
    .on_workspace()
    .separated(Slot::Window),
    Command::new("toggle-right-dock", "Toggle Right Dock", || {
        Box::new(ToggleRightDock)
    })
    .key("cmd-r")
    .on_workspace()
    .menu(Slot::Window),
    // --- bound, but in no menu ---
    //
    // Layered dismissal: escape in a focused input blurs it, the next escape
    // lands on the workspace and puts the inspector away. Not an item, and not
    // a palette row: it is a gesture, not a verb you go looking for by name.
    Command::new("dismiss", "Dismiss", || Box::new(Dismiss))
        .key("escape")
        .on_workspace()
        .out_of_palette(),
];

/// Why a verb that acts on the selected row cannot run.
///
/// `Unknown` — the menu bar — never greys: it cannot rebuild per selection, so
/// legality is re-derived when the item is chosen and the refusal goes to the
/// banner. Everything that *can* grey honestly reuses `row_menu`'s predicates
/// rather than restating them.
fn selection_refusal(facts: Facts, action: RowAction) -> Option<&'static str> {
    match facts.selection {
        Selection::Unknown => None,
        Selection::None => Some("no task is selected"),
        Selection::Task(context) => row_menu::item(context, action).and_then(|item| item.disabled),
    }
}

/// Two concurrent `tasks` runs are refused in `ServerControl::start`; this
/// exists so the surface says why instead of swallowing the click.
fn busy_refusal(facts: Facts) -> Option<&'static str> {
    facts
        .menu
        .busy
        .then_some("a server run is already in flight")
}

fn running_server_refusal(facts: Facts) -> Option<&'static str> {
    busy_refusal(facts).or(match facts.menu.serving {
        true => None,
        false => Some("no server is running"),
    })
}

/// The command with this id, or `None`.
///
/// Ids are unique (`ids_are_unique` pins that), so this is the one lookup.
/// It exists so a surface that *names* a menu item can render the registry's
/// own label rather than a copy of it — see [`crate::autonomy::OFF_SWITCHES`],
/// where a hand-written "Kill All Containers" would silently point at nothing
/// after a rename.
pub fn by_id(id: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|command| command.id == id)
}

/// The items of one top-level menu, greyed and checked against `facts`.
///
/// `MenuItem::Action` is built as a struct literal rather than through
/// `MenuItem::action`, which takes an `impl Action` and so cannot take the
/// boxed action a table has to hand out. That the variant is public is what
/// makes a generated bar possible at all.
pub fn menu_items(slot: Slot, facts: Facts) -> Vec<MenuItem> {
    let mut items = Vec::new();
    for command in COMMANDS.iter().filter(|c| c.menu == Some(slot)) {
        if command.separator_before && !items.is_empty() {
            items.push(MenuItem::Separator);
        }
        items.push(MenuItem::Action {
            name: command.label(facts).into(),
            action: (command.action)(),
            os_action: None,
            checked: command.is_checked(facts),
            disabled: command.refusal_for(facts).is_some(),
        });
    }
    items
}

/// Install every key equivalent the table states.
///
/// Must run before `menus::set`: gpui reads shortcuts out of the keymap while
/// building the bar, once, and a binding installed afterwards shows no key
/// equivalent with nothing to warn you.
///
/// `KeyBinding::load` rather than `KeyBinding::new`, because `new` is generic
/// over a *sized* action and cannot take the `Box<dyn Action>` a table hands
/// out. `load` is the same constructor one layer down, given the same
/// `use_key_equivalents: false` that `new` passes.
pub fn bind_keys(cx: &mut App) {
    let mapper = cx.keyboard_mapper().clone();
    let bindings: Vec<KeyBinding> = COMMANDS
        .iter()
        .filter_map(|command| {
            let key = command.key?;
            let context = command.handler.context().map(|context| {
                Rc::new(
                    KeyBindingContextPredicate::parse(context)
                        .expect("a command's keymap context must parse"),
                )
            });
            Some(
                KeyBinding::load(
                    key,
                    (command.action)(),
                    context,
                    false,
                    None,
                    mapper.as_ref(),
                )
                .expect("a command's key equivalent must parse"),
            )
        })
        .collect();
    cx.bind_keys(bindings);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tasks_client::api::models::{GhState, SpecQueueStatus, TaskState};

    /// The command with this id. Ids are unique — the first test pins that.
    fn command(id: &str) -> Option<&'static Command> {
        super::by_id(id)
    }

    fn facts() -> Facts {
        Facts::for_menu_bar(MenuState::default())
    }

    fn selected(task_state: TaskState) -> Facts {
        row(task_state, None)
    }

    fn row(task_state: TaskState, spec: Option<SpecQueueStatus>) -> Facts {
        Facts {
            menu: MenuState::default(),
            selection: Selection::Task(RowContext {
                task_state,
                gh_state: GhState::Open,
                has_github_url: true,
                spec,
            }),
        }
    }

    #[test]
    fn every_id_is_unique() {
        let mut ids: Vec<&str> = COMMANDS.iter().map(|command| command.id).collect();
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), total, "duplicate command id");
    }

    /// Two commands answering to one keystroke is a keystroke whose meaning
    /// depends on registration order — the class of bug this table exists to
    /// make impossible to introduce silently.
    #[test]
    fn no_keystroke_is_claimed_twice_in_one_context() {
        let mut bound: Vec<(&str, Option<&str>)> = COMMANDS
            .iter()
            .filter_map(|command| Some((command.key?, command.handler.context())))
            .collect();
        let total = bound.len();
        bound.sort_unstable();
        bound.dedup();
        assert_eq!(bound.len(), total, "two commands share a keystroke");
    }

    /// ⇧⌘Q is macOS's own Log Out and the system takes it first.
    #[test]
    fn nothing_collides_with_a_macos_system_binding() {
        for command in COMMANDS {
            assert_ne!(command.key, Some("shift-cmd-q"), "{}", command.id);
            assert_ne!(command.key, Some("ctrl-cmd-q"), "{}", command.id);
        }
    }

    /// The pipeline, the docks, the history steps and the selection verbs all
    /// act on *this window's* state, so they are element-handled and grey out
    /// with no workspace focused. Minimize/Zoom/Hide/Quit are not a window's
    /// business at all.
    #[test]
    fn what_acts_on_the_workspace_is_handled_by_the_workspace() {
        for id in [
            "history-back",
            "toggle-left-dock",
            "toggle-right-dock",
            "queue-selected",
            "mode-play",
            "toggle-show-done",
            "new-issue",
        ] {
            assert_eq!(
                command(id).unwrap().handler,
                Handler::Workspace,
                "{id} should be element-handled"
            );
        }
        for id in [
            "about",
            "quit",
            "minimize",
            "close-window",
            "restart-server",
        ] {
            assert_eq!(command(id).unwrap().handler, Handler::Global, "{id}");
        }
    }

    /// The menu bar is `Selection::Unknown` and never greys on it: it cannot
    /// rebuild per selection, so the refusal is reported when the item is
    /// chosen instead.
    #[test]
    fn the_selection_verbs_never_grey_for_the_menu_bar() {
        for id in ["queue-selected", "scout-selected", "approve-selected"] {
            let command = command(id).unwrap();
            for menu in [
                MenuState::default(),
                MenuState {
                    serving: true,
                    busy: true,
                    mode: Some(Mode::Stop),
                    show_done: true,
                },
            ] {
                assert_eq!(
                    command.refusal_for(Facts::for_menu_bar(menu)),
                    None,
                    "{id} greyed itself in the bar"
                );
            }
        }
    }

    /// Anything that knows the selection greys honestly, reusing the row
    /// menu's own predicates rather than restating them.
    #[test]
    fn the_selection_verbs_grey_honestly_when_the_selection_is_known() {
        let queue = command("queue-selected").unwrap();
        assert_eq!(queue.refusal_for(selected(TaskState::Backlog)), None);
        assert_eq!(
            queue.refusal_for(selected(TaskState::Queued)),
            Some("already queued")
        );
        assert_eq!(
            queue.refusal_for(Facts {
                menu: MenuState::default(),
                selection: Selection::None,
            }),
            Some("no task is selected")
        );

        let scout = command("scout-selected").unwrap();
        assert_eq!(
            scout.refusal_for(selected(TaskState::Scouting)),
            Some("already running")
        );

        let approve = command("approve-selected").unwrap();
        assert_eq!(
            approve.refusal_for(selected(TaskState::Backlog)),
            Some("no spec yet")
        );
        assert_eq!(
            approve.refusal_for(row(
                TaskState::InReview,
                Some(SpecQueueStatus::PendingReview)
            )),
            None
        );
    }

    #[test]
    fn a_running_server_is_what_stopping_and_draining_need() {
        let idle = facts();
        assert_eq!(command("restart-server").unwrap().refusal_for(idle), None);
        for id in [
            "restart-server-when-idle",
            "stop-server",
            "stop-server-when-idle",
        ] {
            assert_eq!(
                command(id).unwrap().refusal_for(idle),
                Some("no server is running"),
                "{id}"
            );
        }

        let busy = Facts::for_menu_bar(MenuState {
            serving: true,
            busy: true,
            ..MenuState::default()
        });
        for id in [
            "restart-server",
            "restart-server-when-idle",
            "stop-server",
            "stop-server-when-idle",
        ] {
            assert_eq!(
                command(id).unwrap().refusal_for(busy),
                Some("a server run is already in flight"),
                "{id}"
            );
        }
        // Reading is always allowed — especially while something is running.
        assert_eq!(command("server-status").unwrap().refusal_for(busy), None);
    }

    /// One item, two names: `tasks reload` with no live pid *is* a start.
    #[test]
    fn the_restart_command_renames_itself_when_nothing_is_serving() {
        let command = command("restart-server").unwrap();
        assert_eq!(command.label(facts()), "Start Server");
        assert_eq!(
            command.label(Facts::for_menu_bar(MenuState {
                serving: true,
                ..MenuState::default()
            })),
            "Restart Server…"
        );
    }

    #[test]
    fn the_pipeline_group_is_a_radio_over_the_live_mode() {
        for (mode, checked) in [
            (Mode::Play, "mode-play"),
            (Mode::Pause, "mode-pause"),
            (Mode::Stop, "mode-stop"),
        ] {
            let facts = Facts::for_menu_bar(MenuState {
                mode: Some(mode),
                ..MenuState::default()
            });
            for id in ["mode-play", "mode-pause", "mode-stop"] {
                assert_eq!(
                    command(id).unwrap().is_checked(facts),
                    id == checked,
                    "{id} with mode {}",
                    mode.as_str()
                );
            }
        }
        // Before the first snapshot, nothing is claimed.
        assert!(!command("mode-play").unwrap().is_checked(facts()));
    }

    /// A separator that opens a menu is a rule above nothing.
    #[test]
    fn no_menu_opens_with_a_separator() {
        for slot in [
            Slot::App,
            Slot::File,
            Slot::View,
            Slot::Task,
            Slot::Server,
            Slot::Window,
        ] {
            let items = menu_items(slot, facts());
            assert!(
                !matches!(items.first(), Some(MenuItem::Separator)),
                "{} opens with a separator",
                slot.menu_name()
            );
        }
    }

    /// The palette's category prefix is derived from the slot, so a command
    /// cannot advertise a category it does not sit in.
    #[test]
    fn a_palette_row_names_the_menu_it_lives_under() {
        assert_eq!(
            command("stop-server").unwrap().palette_label(facts()),
            "Server: Stop Server…"
        );
        assert_eq!(
            command("history-back").unwrap().palette_label(facts()),
            "View: Back"
        );
        // …and it renames itself there too, for the same reason it does in
        // the bar.
        assert_eq!(
            command("restart-server").unwrap().palette_label(facts()),
            "Server: Start Server"
        );
    }

    /// Escape and the two palettes are the only things kept out of the
    /// palette, and each for its own stated reason.
    #[test]
    fn only_the_gestures_and_the_palettes_stay_out_of_the_palette() {
        let out: Vec<&str> = COMMANDS
            .iter()
            .filter(|command| !command.in_palette)
            .map(|command| command.id)
            .collect();
        assert_eq!(out, ["go-to-anything", "command-palette", "dismiss"]);
    }

    /// Everything with a slot reaches the bar — the check that a command
    /// added to the table cannot be silently invisible.
    #[test]
    fn every_command_with_a_slot_reaches_a_menu() {
        let facts = facts();
        let in_bar: Vec<String> = [
            Slot::App,
            Slot::File,
            Slot::View,
            Slot::Task,
            Slot::Server,
            Slot::Window,
        ]
        .into_iter()
        .flat_map(|slot| menu_items(slot, facts))
        .filter_map(|item| match item {
            MenuItem::Action { name, .. } => Some(name.to_string()),
            _ => None,
        })
        .collect();

        for command in COMMANDS.iter().filter(|command| command.menu.is_some()) {
            assert!(
                in_bar.contains(&command.label(facts).to_string()),
                "{} never reaches the bar",
                command.id
            );
        }
        assert_eq!(
            in_bar.len(),
            COMMANDS
                .iter()
                .filter(|command| command.menu.is_some())
                .count()
        );
    }

    /// The keymap context is a property of the handler, not of the command —
    /// one decision point, so a new entry cannot get it half right.
    #[test]
    fn the_binding_context_follows_the_handler() {
        assert_eq!(Handler::Global.context(), None);
        assert_eq!(Handler::Workspace.context(), Some("Workspace"));
        for command in COMMANDS {
            if let Some(context) = command.handler.context() {
                assert!(
                    KeyBindingContextPredicate::parse(context).is_ok(),
                    "{}",
                    command.id
                );
            }
        }
    }

    /// Every keystroke in the table is one gpui can actually parse. `bind_keys`
    /// panics on a bad one, and it runs at startup — this is the same check,
    /// without a gpui `App`.
    #[test]
    fn every_key_equivalent_renders() {
        for command in COMMANDS {
            let Some(key) = command.key else { continue };
            let rendered = command.rendered_key().unwrap();
            assert!(!rendered.is_empty(), "{}", command.id);
            assert!(
                !key.contains(' '),
                "{} binds a chord; the bar renders only single keystrokes",
                command.id
            );
        }
    }
}
