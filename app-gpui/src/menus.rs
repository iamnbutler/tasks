//! The application menu bar.
//!
//! The bar is *derived*, not written: [`menus`] is a fold over
//! [`crate::commands::COMMANDS`], one menu per [`Slot`], with only the Edit
//! menu spliced in by hand — its items dispatch gpuikit's own input actions
//! through `MenuItem::os_action`, which are not this app's actions and are
//! never bound here. Which verbs exist, what they are called, what they are
//! bound to and when they grey out all live in that one table.
//!
//! Two entry points, and the order between them is load-bearing: [`init`]
//! registers the handlers, `commands::bind_keys` installs the bindings, then
//! [`set`] installs the bar. gpui reads shortcuts out of the keymap while
//! building the menu, once — an action bound after `set_menus` shows no key
//! equivalent and nothing warns you about it.
//!
//! Handlers are global (`App::on_action`) rather than hung off the workspace
//! root. Minimize/Zoom/Close act on whichever window is focused — including
//! the About window, which has no workspace behind it — and Hide/Quit aren't a
//! window's business at all. They reach a window through `cx.active_window()`.
//!
//! One consequence worth knowing rather than treating as a bug: menu items
//! grey themselves out via `App::is_action_available`, and an action with a
//! global handler is *always* available. The dock toggles are element-handled
//! (the workspace's `on_action`), so they correctly grey out while the About
//! window is focused, and Cut/Copy/Paste grey out with no text input focused.
//! That is the macOS-correct behaviour, and it is why the workspace's existing
//! handlers were left where they are.
//!
//! The bar is *data*: [`menus`] is a pure function of [`MenuState`], and
//! [`set`] is the only thing that installs it. Rebuilds go through [`sync`],
//! which is guarded on the state actually having changed — `set_menus` leaks
//! (`gpui-macos`'s platform appends every item's action into `menu_actions`
//! and never clears it, so each rebuild leaks a boxed action per item).
//! Indices stay valid, so dispatch keeps working; it is only the leak that
//! makes a per-frame rebuild wrong. Mode, connectivity and busy transitions
//! are rare, so the total is bounded.

use gpui::Global;
use gpui::{actions, App, Menu, MenuItem, OsAction, Window};
use gpuikit::input::bindings;
use tasks_client::api::models::Mode;

use crate::about;
use crate::commands::{self, Facts, Slot};
use crate::server::{self, Op};
use crate::server_window;

actions!(
    tasks,
    [
        About,
        Hide,
        HideOthers,
        ShowAll,
        Quit,
        Minimize,
        Zoom,
        CloseWindow,
        ShowServerStatus,
        RestartServer,
        RestartServerWhenIdle,
        StopServer,
        StopServerWhenIdle,
        RevealServeLog,
        OpenDataDirectory
    ]
);

/// The Task menu's three key equivalents, as the keymap spells them.
///
/// Public because the row menu advertises the same shortcuts next to the same
/// verbs, and it *renders* these rather than restating them — see
/// [`rendered_keystroke`]. Only these three: they are the safe verbs, and
/// nothing that closes an issue is bound, for the same reason nothing in the
/// Server menu is.
///
/// `shift-cmd-u` rather than the mnemonic `shift-cmd-q`, because ⇧⌘Q is macOS's
/// own Log Out shortcut and the system takes it first. "Queue *up*" is the
/// mnemonic that was available.
pub const QUEUE_KEYSTROKE: &str = "shift-cmd-u";
pub const SCOUT_KEYSTROKE: &str = "shift-cmd-s";
pub const APPROVE_KEYSTROKE: &str = "shift-cmd-a";

/// A keymap keystroke (`"shift-cmd-s"`) as macOS writes it (`"⇧⌘S"`),
/// modifiers in the platform's canonical order.
///
/// The menu bar never needs this — gpui reads shortcuts out of the keymap
/// while building the bar — but a gpuikit context-menu item takes its
/// shortcut as text, and a hand-written one is a second string that can
/// drift from the binding. This derives it from the binding instead. Only
/// rich enough for the bindings this app installs.
pub fn rendered_keystroke(keystroke: &str) -> String {
    let parts: Vec<&str> = keystroke.split('-').collect();
    let has = |name: &str| parts.contains(&name);
    let mut rendered = String::new();
    if has("ctrl") || has("control") {
        rendered.push('⌃');
    }
    if has("alt") {
        rendered.push('⌥');
    }
    if has("shift") {
        rendered.push('⇧');
    }
    if has("cmd") || has("platform") {
        rendered.push('⌘');
    }
    if let Some(key) = parts.last() {
        rendered.push_str(&key.to_uppercase());
    }
    rendered
}

/// The facts the bar's shape depends on. Everything else about the menu is
/// fixed at compile time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MenuState {
    /// A server is answering. Drives the Restart/Start label and what can be
    /// stopped.
    pub serving: bool,
    /// The pipeline mode, for the radio group. `None` before the first
    /// snapshot lands.
    pub mode: Option<Mode>,
    /// A `tasks` run is in flight, so a second one would be refused.
    pub busy: bool,
    /// The Tasks list is showing its archive of done tasks, for the View
    /// menu's checkmark. A checkmark rather than a renaming item: the item
    /// names one thing whose state you can read at a glance, which is what
    /// a view filter is.
    pub show_done: bool,
}

/// The state the installed bar was built from.
struct InstalledMenus(MenuState);

impl Global for InstalledMenus {}

/// Register the menu bar's *global* action handlers.
///
/// The key bindings are not here any more — `commands::bind_keys` installs
/// every one of them from the registry, and it must run before [`set`] for the
/// reason the module docs give. What is left here is the half of the handlers
/// that is global on purpose: Minimize/Zoom/Close act on whichever window is
/// focused (including the About window, which has no workspace behind it), and
/// Hide/Quit are not a window's business at all. The workspace's own handlers
/// stay on the workspace root, so they grey out with nothing focused.
pub fn init(cx: &mut App) {
    cx.on_action(|_: &About, cx| about::open(cx));
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &Minimize, cx| with_active_window(cx, |window| window.minimize_window()));
    cx.on_action(|_: &Zoom, cx| with_active_window(cx, |window| window.zoom_window()));
    cx.on_action(|_: &CloseWindow, cx| with_active_window(cx, |window| window.remove_window()));

    // The Server menu. Each of the three process ops opens the Server window
    // *and* starts the run: a menu item that silently kicks off minutes of
    // staged work is a spinner that resolves to nothing.
    cx.on_action(|_: &ShowServerStatus, cx| server_window::open(cx));
    cx.on_action(|_: &RestartServer, cx| server_window::run(cx, Op::Restart));
    cx.on_action(|_: &RestartServerWhenIdle, cx| {
        server_window::run(cx, Op::RestartWhenIdle);
    });
    cx.on_action(|_: &StopServer, cx| server_window::run(cx, Op::Stop));
    cx.on_action(|_: &StopServerWhenIdle, cx| {
        server_window::run(cx, Op::StopWhenIdle);
    });
    cx.on_action(|_: &RevealServeLog, _cx| {
        if let Some(dir) = tasks_api::paths::data_dir() {
            let log = tasks_api::paths::serve_log(&dir);
            // A server that has only ever run in the foreground has no
            // serve.log; revealing the directory still answers "where would
            // it be?".
            match log.is_file() {
                true => server::reveal(&log),
                false => server::open_path(&dir),
            }
        }
    });
    cx.on_action(|_: &OpenDataDirectory, _cx| {
        if let Some(dir) = tasks_api::paths::data_dir() {
            server::open_path(&dir);
        }
    });
}

/// Install the menu bar. Call after [`init`].
pub fn set(cx: &mut App, state: MenuState) {
    cx.set_menus(menus(state));
    cx.set_global(InstalledMenus(state));
}

/// Reinstall the bar if — and only if — `state` differs from what is up.
///
/// The guard is not an optimization: see the module docs on `set_menus`.
pub fn sync(cx: &mut App, state: MenuState) {
    if cx.try_global::<InstalledMenus>().map(|g| g.0) == Some(state) {
        return;
    }
    set(cx, state);
}

/// The bar itself, as data — kept separate from [`set`] so the structure is
/// testable without standing up a gpui `App`.
///
/// Every menu but Edit is a fold over the registry. `Selection::Unknown` is
/// what the bar passes: it cannot grey per selection, because `set_menus`
/// leaks a boxed action per item on every rebuild (see the module docs) and
/// the selection moves on every arrow key. A verb that cannot run therefore
/// says so in the sidebar banner when it is *chosen* — a keystroke that
/// quietly does nothing reads as a bug, the reason reads as an answer.
fn menus(state: MenuState) -> Vec<Menu> {
    let facts = Facts::for_menu_bar(state);
    let menu = |slot: Slot| Menu::new(slot.menu_name()).items(commands::menu_items(slot, facts));
    vec![
        menu(Slot::App),
        menu(Slot::File),
        edit_menu(),
        menu(Slot::View),
        menu(Slot::Task),
        // The one menu that acts on the server *process* rather than over
        // HTTP, because a server cannot gracefully swap itself out through
        // its own API.
        menu(Slot::Server),
        // The name is load-bearing: gpui special-cases the literal string
        // "Window" and hands that menu to AppKit as the windows menu, which is
        // what makes the list of open windows append itself. Renaming it
        // silently loses that — which is why `Slot::Window::menu_name` is not
        // free to change either.
        menu(Slot::Window),
    ]
}

/// The one menu the registry does not generate.
///
/// These dispatch gpuikit's *own* input actions rather than new ones, so the
/// items drive exactly the code path `cmd-c` already drove and every present
/// and future `InputState` is covered without opting in. `os_action` maps them
/// to the AppKit selectors, which keeps the key equivalents working inside
/// system panels. Undo/Redo are the exception gpui documents: with no
/// `NSTextView` behind them `undo:` and `redo:` are permanently disabled, so
/// gpui routes those two back through its own dispatch and they behave like
/// plain actions.
fn edit_menu() -> Menu {
    Menu::new("Edit").items([
        MenuItem::os_action("Undo", bindings::Undo, OsAction::Undo),
        MenuItem::os_action("Redo", bindings::Redo, OsAction::Redo),
        MenuItem::separator(),
        MenuItem::os_action("Cut", bindings::Cut, OsAction::Cut),
        MenuItem::os_action("Copy", bindings::Copy, OsAction::Copy),
        MenuItem::os_action("Paste", bindings::Paste, OsAction::Paste),
        MenuItem::separator(),
        MenuItem::os_action("Select All", bindings::SelectAll, OsAction::SelectAll),
    ])
}

fn with_active_window(cx: &mut App, f: impl FnOnce(&mut Window)) {
    if let Some(handle) = cx.active_window() {
        // Fails only if the window closed out from under the menu.
        handle.update(cx, |_, window, _| f(window)).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The items of the named top-level menu, as (label, action name) pairs
    /// with separators dropped.
    fn items_of(state: MenuState, name: &str) -> Vec<(String, &'static str)> {
        let menus = menus(state);
        let menu = menus
            .iter()
            .find(|menu| menu.name == name)
            .unwrap_or_else(|| panic!("no {name} menu"));
        menu.items
            .iter()
            .filter_map(|item| match item {
                MenuItem::Action { name, action, .. } => Some((name.to_string(), action.name())),
                _ => None,
            })
            .collect()
    }

    fn items(name: &str) -> Vec<(String, &'static str)> {
        items_of(MenuState::default(), name)
    }

    /// The one item in the named menu whose label is `label`.
    fn item(state: MenuState, menu_name: &str, label: &str) -> MenuItem {
        menus(state)
            .into_iter()
            .find(|menu| menu.name == menu_name)
            .unwrap_or_else(|| panic!("no {menu_name} menu"))
            .items
            .into_iter()
            .find(|item| matches!(item, MenuItem::Action { name, .. } if name.as_ref() == label))
            .unwrap_or_else(|| panic!("no {label:?} item"))
    }

    fn serving(mode: Mode) -> MenuState {
        MenuState {
            serving: true,
            mode: Some(mode),
            ..MenuState::default()
        }
    }

    #[test]
    fn bar_has_the_seven_menus_in_order() {
        let names: Vec<_> = menus(MenuState::default())
            .iter()
            .map(|menu| menu.name.to_string())
            .collect();
        assert_eq!(
            names,
            ["Tasks", "File", "Edit", "View", "Task", "Server", "Window"]
        );
    }

    /// The three safe verbs, and only those. Closing an issue is a row-menu
    /// click with no menu-bar item and no keystroke anywhere.
    #[test]
    fn the_task_menu_carries_the_three_safe_verbs() {
        let actions: Vec<_> = items("Task").into_iter().map(|(_, a)| a).collect();
        assert_eq!(
            actions,
            [
                "workspace::QueueSelectedTask",
                "workspace::ScoutSelectedTask",
                "workspace::ApproveSelectedSpec",
            ]
        );
    }

    /// Nothing in the Task menu greys itself: the bar cannot rebuild per
    /// selection, so legality is re-derived when the item is chosen.
    #[test]
    fn the_task_menu_never_greys_itself() {
        for state in [
            MenuState::default(),
            serving(Mode::Play),
            MenuState {
                busy: true,
                ..serving(Mode::Stop)
            },
        ] {
            for label in ["Add to Queue", "Scout Now", "Approve Spec"] {
                assert!(!item(state, "Task", label).is_disabled(), "{label}");
            }
        }
    }

    #[test]
    fn keystrokes_render_the_way_macos_writes_them() {
        assert_eq!(rendered_keystroke("shift-cmd-u"), "⇧⌘U");
        assert_eq!(rendered_keystroke("shift-cmd-s"), "⇧⌘S");
        assert_eq!(rendered_keystroke("cmd-1"), "⌘1");
        // Canonical order, whatever order the keymap spelled it in.
        assert_eq!(rendered_keystroke("cmd-shift-alt-ctrl-k"), "⌃⌥⇧⌘K");
    }

    /// ⇧⌘Q is macOS's Log Out, so the mnemonic one is not available; this is
    /// the check that keeps it from creeping back in.
    #[test]
    fn no_task_shortcut_collides_with_a_macos_system_binding() {
        for keystroke in [QUEUE_KEYSTROKE, SCOUT_KEYSTROKE, APPROVE_KEYSTROKE] {
            assert_ne!(keystroke, "shift-cmd-q");
            assert_ne!(keystroke, "ctrl-cmd-q");
        }
    }

    /// Status first: you should be able to see what you are about to
    /// interrupt before you interrupt it.
    #[test]
    fn the_server_menu_leads_with_status_and_carries_every_op() {
        let actions: Vec<_> = items_of(serving(Mode::Play), "Server")
            .into_iter()
            .map(|(_, a)| a)
            .collect();
        assert_eq!(
            actions,
            [
                "tasks::ShowServerStatus",
                "tasks::RestartServer",
                "tasks::RestartServerWhenIdle",
                "tasks::StopServer",
                "tasks::StopServerWhenIdle",
                "workspace::SetModePlay",
                "workspace::SetModePause",
                "workspace::SetModeStop",
                "workspace::KillAllContainers",
                "tasks::RevealServeLog",
                "tasks::OpenDataDirectory",
            ]
        );
    }

    /// One item, two names: `tasks reload` with no live pid *is* a start, so
    /// a second action would run the identical command.
    #[test]
    fn the_restart_item_renames_itself_when_nothing_is_serving() {
        let labels: Vec<_> = items_of(serving(Mode::Play), "Server")
            .into_iter()
            .map(|(label, _)| label)
            .collect();
        assert!(
            labels.contains(&"Restart Server…".to_string()),
            "{labels:?}"
        );

        let labels: Vec<_> = items("Server")
            .into_iter()
            .map(|(label, _)| label)
            .collect();
        assert!(labels.contains(&"Start Server".to_string()), "{labels:?}");
        assert!(
            !labels.contains(&"Restart Server…".to_string()),
            "{labels:?}"
        );
    }

    /// With nothing up, waiting for a drain point has nothing to wait for and
    /// a stop has nothing to stop — but starting one is exactly the point.
    #[test]
    fn what_needs_a_running_server_greys_out_without_one() {
        let idle = MenuState::default();
        assert!(!item(idle, "Server", "Start Server").is_disabled());
        assert!(item(idle, "Server", "Restart When Idle…").is_disabled());
        assert!(item(idle, "Server", "Stop Server…").is_disabled());
        assert!(item(idle, "Server", "Stop When Idle…").is_disabled());
    }

    /// The two pairs read the same way round: an immediate verb and a patient
    /// one beside it, both of which may now ask before they act.
    #[test]
    fn stopping_offers_the_same_pair_restarting_does() {
        let labels: Vec<_> = items_of(serving(Mode::Play), "Server")
            .into_iter()
            .map(|(label, _)| label)
            .collect();
        let index = |label: &str| {
            labels
                .iter()
                .position(|item| item == label)
                .unwrap_or_else(|| panic!("no {label:?} item in {labels:?}"))
        };
        assert!(index("Restart Server…") < index("Restart When Idle…"));
        assert!(index("Restart When Idle…") < index("Stop Server…"));
        assert!(index("Stop Server…") < index("Stop When Idle…"));
    }

    /// Two concurrent runs are refused in `ServerControl::start`; the greying
    /// exists so the menu says why instead of swallowing the click.
    #[test]
    fn a_run_in_flight_greys_out_every_process_op() {
        let busy = MenuState {
            busy: true,
            ..serving(Mode::Play)
        };
        assert!(item(busy, "Server", "Restart Server…").is_disabled());
        assert!(item(busy, "Server", "Restart When Idle…").is_disabled());
        assert!(item(busy, "Server", "Stop Server…").is_disabled());
        assert!(item(busy, "Server", "Stop When Idle…").is_disabled());
        // Reading is always allowed — especially while something is running.
        assert!(!item(busy, "Server", "Server Status…").is_disabled());
    }

    #[test]
    fn the_pipeline_group_is_a_radio_group_over_the_live_mode() {
        for (mode, checked) in [
            (Mode::Play, "Pipeline: Play"),
            (Mode::Pause, "Pipeline: Pause"),
            (Mode::Stop, "Pipeline: Stop"),
        ] {
            let state = serving(mode);
            for label in ["Pipeline: Play", "Pipeline: Pause", "Pipeline: Stop"] {
                assert_eq!(
                    item(state, "Server", label).is_checked(),
                    label == checked,
                    "{label} with mode {}",
                    mode.as_str()
                );
            }
        }
        // Before the first snapshot, nothing is claimed.
        let unknown = MenuState {
            serving: true,
            ..MenuState::default()
        };
        assert!(!item(unknown, "Server", "Pipeline: Play").is_checked());
    }

    /// Whatever the state, the menu offers each op exactly once — the
    /// renaming item is one item, not a pair that can both show up.
    #[test]
    fn no_state_puts_the_same_op_in_the_menu_twice() {
        for state in [
            MenuState::default(),
            serving(Mode::Play),
            MenuState {
                busy: true,
                ..serving(Mode::Stop)
            },
        ] {
            let mut actions: Vec<_> = items_of(state, "Server")
                .into_iter()
                .map(|(_, a)| a)
                .collect();
            let total = actions.len();
            actions.sort_unstable();
            actions.dedup();
            assert_eq!(actions.len(), total, "{actions:?}");
        }
    }

    /// History first, then the two palettes — because a surface reachable
    /// only by knowing its keystroke is one most people never find — then the
    /// archive filter. Sections died with the v3 frame swap.
    #[test]
    fn view_menu_covers_history_palettes_and_the_archive_filter() {
        let actions: Vec<_> = items("View").into_iter().map(|(_, a)| a).collect();
        assert_eq!(
            actions,
            [
                "workspace::HistoryBack",
                "workspace::HistoryForward",
                "palette::GoToAnything",
                "palette::ShowCommandPalette",
                "workspace::ToggleShowDone",
            ]
        );
    }

    /// One item that reads its own state, not two that say opposite things —
    /// the archive is a filter you can see the position of.
    #[test]
    fn the_archive_toggle_is_a_checkmark_over_the_live_filter() {
        let hidden = MenuState::default();
        assert!(!item(hidden, "View", "Show Done Tasks").is_checked());

        let shown = MenuState {
            show_done: true,
            ..MenuState::default()
        };
        assert!(item(shown, "View", "Show Done Tasks").is_checked());
        // Still one item, still the same action — nothing renamed itself.
        let labels: Vec<_> = items_of(shown, "View")
            .into_iter()
            .map(|(label, _)| label)
            .collect();
        assert_eq!(
            labels.iter().filter(|l| l.contains("Done")).count(),
            1,
            "{labels:?}"
        );
    }

    #[test]
    fn app_menu_opens_with_about_and_ends_with_quit() {
        let items = items("Tasks");
        assert_eq!(items.first().unwrap().0, "About Tasks");
        assert_eq!(items.first().unwrap().1, "tasks::About");
        assert_eq!(items.last().unwrap().1, "tasks::Quit");
    }

    #[test]
    fn window_menu_carries_the_dock_toggles() {
        let actions: Vec<_> = items("Window").into_iter().map(|(_, a)| a).collect();
        assert!(actions.contains(&"workspace::ToggleLeftDock"));
        assert!(actions.contains(&"workspace::ToggleRightDock"));
    }

    /// gpui hands the menu literally named "Window" to AppKit as the windows
    /// menu; renaming it silently drops the open-windows list.
    #[test]
    fn window_menu_keeps_its_platform_name() {
        assert!(menus(MenuState::default())
            .iter()
            .any(|menu| menu.name == "Window"));
    }

    #[test]
    fn edit_menu_dispatches_gpuikits_input_actions() {
        let actions: Vec<_> = items("Edit").into_iter().map(|(_, a)| a).collect();
        assert_eq!(
            actions,
            [
                "input::Undo",
                "input::Redo",
                "input::Cut",
                "input::Copy",
                "input::Paste",
                "input::SelectAll",
            ]
        );
    }

    /// New Issue joined this menu when the window that creates one landed —
    /// the comment that used to sit here said it should. Add Repo joined it
    /// with multi-repo, for the same reason: a surface reachable only from a
    /// popover in the title bar is one most people never find.
    #[test]
    fn file_menu_only_offers_what_exists() {
        let actions: Vec<_> = items("File").into_iter().map(|(_, a)| a).collect();
        assert_eq!(
            actions,
            [
                "workspace::NewIssue",
                "workspace::AddRepo",
                "tasks::CloseWindow"
            ]
        );
    }

    /// The bar is a fold over the registry, so nothing in the registry can be
    /// invisible in the bar and nothing in the bar can be absent from the
    /// registry. This is the check that a command added to the table cannot
    /// quietly fail to appear — the failure mode the old hand-written lists
    /// had in both directions.
    #[test]
    fn every_registry_command_with_a_slot_reaches_the_bar() {
        let in_bar: Vec<&'static str> = menus(MenuState::default())
            .iter()
            // Edit is spliced in by hand and dispatches gpuikit's actions.
            .filter(|menu| menu.name != "Edit")
            .flat_map(|menu| menu.items.iter())
            .filter_map(|item| match item {
                MenuItem::Action { action, .. } => Some(action.name()),
                _ => None,
            })
            .collect();

        let expected: Vec<&'static str> = commands::COMMANDS
            .iter()
            .filter(|command| command.menu.is_some())
            .map(|command| (command.action)().name())
            .collect();
        assert_eq!(in_bar, expected);

        // …and every one of them still says something.
        for menu in menus(MenuState::default()) {
            for item in &menu.items {
                if let MenuItem::Action { name, .. } = item {
                    assert!(!name.is_empty(), "{} has a nameless item", menu.name);
                }
            }
        }
    }
}
