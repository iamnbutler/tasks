mod components;
mod issue_composer;
mod sections;
mod state;
mod time;
mod workspace;

use gpui::{
    point, px, size, App, AppContext, Application, Bounds, KeyBinding, TitlebarOptions,
    WindowBounds, WindowOptions,
};
use gpuikit::input::bind_input_keys;

use workspace::{NewIssue, ToggleLeftDock, ToggleRightDock, Workspace};

fn main() {
    Application::with_platform(gpui_platform::current_platform(false))
        .with_assets(gpuikit::assets())
        .run(|cx: &mut App| {
            gpuikit::theme::init(cx);
            bind_input_keys(cx, None);

            // Dock toggles match Zed's defaults.
            let ws = Some("Workspace");
            cx.bind_keys([
                KeyBinding::new("cmd-b", ToggleLeftDock, ws),
                KeyBinding::new("cmd-r", ToggleRightDock, ws),
                KeyBinding::new("cmd-alt-b", ToggleRightDock, ws),
                KeyBinding::new("cmd-n", NewIssue, ws),
            ]);

            cx.open_window(
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
            cx.activate(true);
        });
}
