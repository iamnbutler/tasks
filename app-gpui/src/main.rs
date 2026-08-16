mod about;
mod chat_log;
mod components;
mod issue_composer;
mod menus;
mod sections;
mod server;
mod server_window;
mod state;
mod time;
mod workspace;

use gpui::{
    point, px, size, App, AppContext, Application, Bounds, Global, KeyBinding, TitlebarOptions,
    WindowBounds, WindowHandle, WindowOptions,
};
use gpuikit::input::bind_input_keys;

use workspace::{
    Dismiss, GoToActivity, GoToChat, GoToHome, GoToQueue, GoToTasks, NewIssue, ToggleLeftDock,
    ToggleRightDock, ToggleShowDone, Workspace,
};

/// The main window, so `Close Window` can't strand the app: clicking the Dock
/// icon reopens it rather than stacking a second one.
struct WorkspaceWindow(WindowHandle<Workspace>);

impl Global for WorkspaceWindow {}

fn main() {
    let app = Application::with_platform(gpui_platform::current_platform(false))
        .with_assets(gpuikit::assets());

    // macOS fires reopen only when *no* windows are open, so this covers the
    // normal case after cmd-w. Residual gap: close the main window with About
    // still open and the Dock click won't fire; closing About then clicking
    // the Dock recovers. `on_reopen` returns `&Self`, so it can't be chained
    // into `run`.
    app.on_reopen(open_workspace);

    app.run(|cx: &mut App| {
        gpuikit::theme::init(cx);
        bind_input_keys(cx, None);

        // Dock toggles match Zed's defaults.
        let ws = Some("Workspace");
        cx.bind_keys([
            KeyBinding::new("cmd-b", ToggleLeftDock, ws),
            KeyBinding::new("cmd-r", ToggleRightDock, ws),
            KeyBinding::new("cmd-n", NewIssue, ws),
            KeyBinding::new("escape", Dismiss, ws),
            // Section switching, in sidebar order.
            KeyBinding::new("cmd-1", GoToHome, ws),
            KeyBinding::new("cmd-2", GoToTasks, ws),
            KeyBinding::new("cmd-3", GoToQueue, ws),
            KeyBinding::new("cmd-4", GoToActivity, ws),
            KeyBinding::new("cmd-5", GoToChat, ws),
            // The Tasks list's archive. Bound here rather than in
            // `menus::init` only for company — what matters is that it is
            // bound *before* `menus::set` below, or the View item shows no
            // key equivalent and nothing warns you.
            KeyBinding::new("shift-cmd-d", ToggleShowDone, ws),
        ]);

        // The Server menu's model is a global the menu's handlers and the
        // workspace both reach, so it exists before either.
        server::init(cx);

        // Bindings before the bar: gpui reads shortcuts out of the keymap
        // while building the menu, once.
        //
        // The bar starts from the default state — nothing serving, no mode —
        // and the workspace's first `sync_menus` corrects it. A loopback
        // connect is sub-millisecond, so that is one frame in practice, but
        // it does mean the item reads "Start Server" for that frame even
        // with a server up.
        menus::init(cx);
        menus::set(cx, menus::MenuState::default());

        open_workspace(cx);
    });
}

/// Open the main window, or raise the one that is already open.
fn open_workspace(cx: &mut App) {
    // The handle stays structurally valid after its window closes, so a stale
    // one is only detectable by `update` failing.
    if let Some(existing) = cx.try_global::<WorkspaceWindow>().map(|global| global.0) {
        if existing
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
        {
            cx.activate(true);
            return;
        }
    }

    let handle = cx
        .open_window(
            WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("Tasks".into()),
                    appears_transparent: true,
                    // Per the design spec: 8px inset, vertically
                    // centered in the 28px title bar.
                    traffic_light_position: Some(point(px(8.), px(8.))),
                }),
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1100.), px(720.)),
                    cx,
                ))),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Workspace::new(window, cx)),
        )
        .unwrap();
    cx.set_global(WorkspaceWindow(handle));
    cx.activate(true);
}
