//! Tasks Desktop - Main entry point for the GPUI desktop application.

use gpui::{px, size, AppContext, Application, Bounds, WindowBounds, WindowOptions};
use tasks_desktop::RootView;

fn main() {
    Application::new().run(|cx| {
        let bounds = Bounds::centered(None, size(px(800.), px(600.)), cx);

        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| RootView::new("Tasks")),
        )
        .expect("Failed to open window");

        cx.activate(true);
    });
}
