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

use gpui::{actions, App, KeyBinding, Menu, MenuItem, OsAction, Window};
use gpuikit::input::bindings;

use crate::about;
use crate::workspace::{
    GoToActivity, GoToChat, GoToHome, GoToQueue, GoToTasks, ToggleLeftDock, ToggleRightDock,
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
        CloseWindow
    ]
);

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

    // The App and Window menus' shortcuts. The Edit menu's belong to
    // gpuikit's input bindings, which `bind_input_keys` already installed,
    // and the dock toggles' are bound in `main` next to their context.
    cx.bind_keys([
        KeyBinding::new("cmd-h", Hide, None),
        KeyBinding::new("cmd-alt-h", HideOthers, None),
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-m", Minimize, None),
        KeyBinding::new("cmd-w", CloseWindow, None),
    ]);
}

/// Install the menu bar. Call after [`init`].
pub fn set(cx: &mut App) {
    cx.set_menus(menus());
}

/// The bar itself, as data — kept separate from [`set`] so the structure is
/// testable without standing up a gpui `App`.
fn menus() -> Vec<Menu> {
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
    fn items(name: &str) -> Vec<(String, &'static str)> {
        let menus = menus();
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

    #[test]
    fn bar_has_the_five_menus_in_order() {
        let names: Vec<_> = menus().iter().map(|menu| menu.name.to_string()).collect();
        assert_eq!(names, ["Tasks", "File", "Edit", "View", "Window"]);
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
        assert!(menus().iter().any(|menu| menu.name == "Window"));
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
