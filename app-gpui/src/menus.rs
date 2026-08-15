//! The application menu bar.
//!
//! Two entry points, and the order between them is load-bearing: [`init`]
//! registers the handlers *and the key bindings*, then [`set`] installs the
//! bar. gpui reads shortcuts out of the keymap while building the menu, once —
//! an action bound after `set_menus` shows no key equivalent and nothing warns
//! you about it.
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
use gpui::{actions, App, KeyBinding, Menu, MenuItem, OsAction, Window};
use gpuikit::input::bindings;
use tasks_client::api::models::Mode;

use crate::about;
use crate::server::{self, Op};
use crate::server_window;
use crate::workspace::{
    GoToActivity, GoToChat, GoToHome, GoToQueue, GoToTasks, SetModePause, SetModePlay, SetModeStop,
    ToggleLeftDock, ToggleRightDock,
};

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
        RevealServeLog,
        OpenDataDirectory
    ]
);

/// The three facts the bar's shape depends on. Everything else about the menu
/// is fixed at compile time.
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
}

/// The state the installed bar was built from.
struct InstalledMenus(MenuState);

impl Global for InstalledMenus {}

/// Register the menu bar's action handlers and key bindings.
///
/// Must run before [`set`], and after `bind_input_keys` and the dock
/// bindings — that ordering is the whole constraint in `main`.
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

    // The App and Window menus' shortcuts. The Edit menu's belong to
    // gpuikit's input bindings, which `bind_input_keys` already installed,
    // and the dock toggles' are bound in `main` next to their context.
    //
    // Nothing in the Server menu is bound, deliberately: a one-keystroke
    // server restart is the foot-gun this menu is trying not to build.
    // Whoever adds one later must add it *here*, before `set` — gpui reads
    // shortcuts out of the keymap once, while building the bar, and a binding
    // installed afterwards shows no key equivalent with nothing to warn you.
    cx.bind_keys([
        KeyBinding::new("cmd-h", Hide, None),
        KeyBinding::new("cmd-alt-h", HideOthers, None),
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-m", Minimize, None),
        KeyBinding::new("cmd-w", CloseWindow, None),
    ]);
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
fn menus(state: MenuState) -> Vec<Menu> {
    vec![
        Menu::new("Tasks").items([
            MenuItem::action("About Tasks", About),
            MenuItem::separator(),
            MenuItem::action("Hide Tasks", Hide),
            MenuItem::action("Hide Others", HideOthers),
            MenuItem::action("Show All", ShowAll),
            MenuItem::separator(),
            MenuItem::action("Quit Tasks", Quit),
        ]),
        Menu::new("File").items([
            // `New Issue` goes here when the window that creates one lands —
            // one `MenuItem::action` line plus its `cmd-n` binding in `init`.
            MenuItem::action("Close Window", CloseWindow),
        ]),
        // These dispatch gpuikit's *own* input actions rather than new ones,
        // so the items drive exactly the code path `cmd-c` already drove and
        // every present and future `InputState` is covered without opting in.
        // `os_action` maps them to the AppKit selectors, which keeps the key
        // equivalents working inside system panels. Undo/Redo are the
        // exception gpui documents: with no `NSTextView` behind them `undo:`
        // and `redo:` are permanently disabled, so gpui routes those two back
        // through its own dispatch and they behave like plain actions.
        Menu::new("Edit").items([
            MenuItem::os_action("Undo", bindings::Undo, OsAction::Undo),
            MenuItem::os_action("Redo", bindings::Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", bindings::Cut, OsAction::Cut),
            MenuItem::os_action("Copy", bindings::Copy, OsAction::Copy),
            MenuItem::os_action("Paste", bindings::Paste, OsAction::Paste),
            MenuItem::separator(),
            MenuItem::os_action("Select All", bindings::SelectAll, OsAction::SelectAll),
        ]),
        // Section navigation. The key equivalents (⌘1–⌘5) come from the
        // workspace bindings in `main`, which run before `set` — the same
        // ordering constraint as everything else in this bar.
        Menu::new("View").items([
            MenuItem::action("Home", GoToHome),
            MenuItem::action("Tasks", GoToTasks),
            MenuItem::action("Queue", GoToQueue),
            MenuItem::action("Activity", GoToActivity),
            MenuItem::action("Chat", GoToChat),
        ]),
        // The one menu that acts on the server *process* rather than over
        // HTTP, because a server cannot gracefully swap itself out through
        // its own API.
        //
        // Status comes first so you can see what you are about to interrupt.
        // The pipeline group is last-but-one and prefixed, because it governs
        // dispatch rather than the process — same menu, different subject,
        // and the prefix is what keeps "Stop Server" and "Pipeline: Stop"
        // from reading as two spellings of one thing.
        Menu::new("Server").items([
            MenuItem::action("Server Status…", ShowServerStatus),
            MenuItem::separator(),
            // `tasks reload` with no live pid already *is* a start, so this
            // is one item that renames itself rather than two that run the
            // same command.
            match state.serving {
                true => MenuItem::action("Restart Server…", RestartServer),
                false => MenuItem::action("Start Server", RestartServer),
            }
            .disabled(state.busy),
            // `--when-idle` and `stop` both need something to be running:
            // with nothing up, the first has nothing to wait for and the
            // second nothing to stop.
            MenuItem::action("Restart When Idle…", RestartServerWhenIdle)
                .disabled(state.busy || !state.serving),
            MenuItem::action("Stop Server", StopServer).disabled(state.busy || !state.serving),
            MenuItem::separator(),
            MenuItem::action("Pipeline: Play", SetModePlay).checked(state.mode == Some(Mode::Play)),
            MenuItem::action("Pipeline: Pause", SetModePause)
                .checked(state.mode == Some(Mode::Pause)),
            MenuItem::action("Pipeline: Stop", SetModeStop).checked(state.mode == Some(Mode::Stop)),
            MenuItem::separator(),
            MenuItem::action("Reveal serve.log", RevealServeLog),
            MenuItem::action("Open Data Directory", OpenDataDirectory),
        ]),
        // The name is load-bearing: gpui special-cases the literal string
        // "Window" and hands that menu to AppKit as the windows menu, which is
        // what makes the list of open windows append itself. Renaming it
        // silently loses that.
        Menu::new("Window").items([
            MenuItem::action("Minimize", Minimize),
            MenuItem::action("Zoom", Zoom),
            MenuItem::separator(),
            MenuItem::action("Toggle Left Dock", ToggleLeftDock),
            MenuItem::action("Toggle Right Dock", ToggleRightDock),
        ]),
    ]
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
            busy: false,
        }
    }

    #[test]
    fn bar_has_the_six_menus_in_order() {
        let names: Vec<_> = menus(MenuState::default())
            .iter()
            .map(|menu| menu.name.to_string())
            .collect();
        assert_eq!(names, ["Tasks", "File", "Edit", "View", "Server", "Window"]);
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
                "workspace::SetModePlay",
                "workspace::SetModePause",
                "workspace::SetModeStop",
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
        assert!(item(idle, "Server", "Stop Server").is_disabled());
    }

    /// Two concurrent runs are refused in `ServerControl::start`; the greying
    /// exists so the menu says why instead of swallowing the click.
    #[test]
    fn a_run_in_flight_greys_out_every_process_op() {
        let busy = MenuState {
            serving: true,
            mode: Some(Mode::Play),
            busy: true,
        };
        assert!(item(busy, "Server", "Restart Server…").is_disabled());
        assert!(item(busy, "Server", "Restart When Idle…").is_disabled());
        assert!(item(busy, "Server", "Stop Server").is_disabled());
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
            mode: None,
            busy: false,
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
                serving: true,
                mode: Some(Mode::Stop),
                busy: true,
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

    #[test]
    fn view_menu_covers_every_section_in_sidebar_order() {
        let actions: Vec<_> = items("View").into_iter().map(|(_, a)| a).collect();
        assert_eq!(
            actions,
            [
                "workspace::GoToHome",
                "workspace::GoToTasks",
                "workspace::GoToQueue",
                "workspace::GoToActivity",
                "workspace::GoToChat",
            ]
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

    #[test]
    fn file_menu_only_offers_what_exists() {
        let actions: Vec<_> = items("File").into_iter().map(|(_, a)| a).collect();
        assert_eq!(actions, ["tasks::CloseWindow"]);
    }
}
