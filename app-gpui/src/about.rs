//! The About window — the app's name, the build it came from, and what this
//! thing does when you turn it on.
//!
//! Same information set the Swift about panel showed: a marketing version and
//! a monospaced commit line. Both are stamped by `build.rs`; `0.1.0` with
//! `commit unknown` means the binary was built without git in reach.
//!
//! Beneath them, [`crate::disclaimer`]. This is the window a stranger opens
//! before pointing the pipeline at their repositories, so it is where the
//! plain statement belongs. Left-aligned and width-bounded on purpose:
//! centred prose past one line reads as a splash screen, which is the
//! register this must not have.

use gpui::prelude::*;
use gpui::{
    div, px, size, App, Bounds, Context, Global, TitlebarOptions, Window, WindowBounds,
    WindowHandle, WindowOptions,
};
use gpuikit::theme::{ActiveTheme, Themeable};

use crate::disclaimer;

/// The app's mark, inline as SVG bytes — the same double diamond
/// `app-gpui/AppIcon.icns` carries, generated from the same constants by
/// `app-gpui/icon/appicon.py`. Two renderings of one set of numbers rather
/// than two pictures somebody has to remember to update together.
///
/// The *tight* variant: `AppIcon.svg` is the full 1024 macOS grid with the
/// field inset 100 on every side, which the `.icns` requires and which inline
/// would leave the mark sitting optically small and misaligned against the
/// text beside it. Same art, viewBox cropped to the field.
///
/// `include_bytes!` rather than an `AssetSource` entry: this app installs
/// gpuikit's asset source and has none of its own, and inline art is the
/// existing idiom — `workspace::MIC_SVG` is the pattern.
const MARK_SVG: &[u8] = include_bytes!("../icon/AppIconMark.svg");

/// `0.1.<commit count>`, or the crate version with no git in reach.
pub const VERSION: &str = env!("TASKS_GPUI_VERSION");
/// Short SHA (`-dirty` when the tree had uncommitted changes), or `unknown`.
pub const COMMIT: &str = env!("TASKS_GPUI_COMMIT");

/// The About window is a singleton: a second "About Tasks" raises the one
/// that's already open rather than stacking another.
struct AboutWindow(WindowHandle<About>);

impl Global for AboutWindow {}

pub struct About;

/// Open the About window, or raise it if it is already open.
pub fn open(cx: &mut App) {
    // A `WindowHandle` stays structurally valid after its window closes, so
    // the only way to tell a stale one apart is that `update` fails.
    if let Some(existing) = cx.try_global::<AboutWindow>().map(|global| global.0) {
        if existing
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
        {
            cx.activate(true);
            return;
        }
    }

    let options = WindowOptions {
        // A standard title bar, unlike the workspace's: this window draws no
        // chrome of its own, so let AppKit draw the title and close button.
        titlebar: Some(TitlebarOptions {
            title: Some("About Tasks".into()),
            appears_transparent: false,
            traffic_light_position: None,
        }),
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(380.), px(300.)),
            cx,
        ))),
        is_resizable: false,
        is_minimizable: false,
        ..Default::default()
    };

    match cx.open_window(options, |_window, cx| cx.new(|_cx| About)) {
        Ok(handle) => {
            cx.set_global(AboutWindow(handle));
            cx.activate(true);
        }
        // Not worth taking the app down over; the menu item just does nothing.
        Err(error) => eprintln!("failed to open the About window: {error}"),
    }
}

impl Render for About {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let mark = std::sync::Arc::new(gpui::Image::from_bytes(
            gpui::ImageFormat::Svg,
            MARK_SVG.to_vec(),
        ));

        div()
            .flex()
            .flex_col()
            .items_start()
            .justify_center()
            .gap(px(6.))
            .size_full()
            .p(px(20.))
            .bg(theme.bg())
            .font_family(crate::workspace::FONT)
            // A row, not a stack: the window is a fixed 380x300 and is not
            // resizable, so an icon *above* the name would push
            // README_POINTER off the bottom. Beside them it costs the three
            // text lines' own height and nothing more.
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(12.))
                    .child(gpui::img(mark).size(px(56.)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(div().text_color(theme.fg()).child("Tasks"))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme.fg_muted())
                                    .child(format!("Version {VERSION}")),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.fg_muted())
                                    .child(format!("commit {COMMIT}")),
                            ),
                    ),
            )
            .child(
                div()
                    .mt(px(6.))
                    .max_w(px(320.))
                    .text_sm()
                    .text_color(theme.fg())
                    .child(disclaimer::HEADLINE),
            )
            .child(
                div()
                    .max_w(px(320.))
                    .text_xs()
                    .text_color(theme.fg_muted())
                    .child(disclaimer::SUMMARY),
            )
            .child(
                div()
                    .max_w(px(320.))
                    .text_xs()
                    .text_color(theme.fg_muted())
                    .child(disclaimer::README_POINTER),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `env!` already fails the build when `build.rs` doesn't emit these, but
    /// an *empty* value compiles fine and renders as a blank line.
    #[test]
    fn the_build_stamp_says_something() {
        assert!(!VERSION.is_empty());
        assert!(!COMMIT.is_empty());
    }

    /// `include_bytes!` fails the build when the file is missing and says
    /// nothing about whether what it found is art. This is the only test that
    /// covers the mark actually reaching this window, and `.tasks/verify` does
    /// not run it — `app-gpui` is not a workspace member, so `make app-test`
    /// is what does.
    ///
    /// Structure only: no path count, no colour. The generator's constants are
    /// the whole design surface, and a redesign must not have to come here —
    /// the mark stays free to change and only stops being free to stop being
    /// an SVG. `crates/tasks/tests/app_icon.rs` holds the rest.
    #[test]
    fn the_mark_is_an_svg() {
        let mark = std::str::from_utf8(MARK_SVG).expect("the mark is UTF-8");
        assert!(mark.starts_with("<svg "), "not an svg root: {mark:.40}");
        assert!(mark.trim_end().ends_with("</svg>"), "truncated: {mark:.40}");
        assert!(mark.contains("<path "), "the mark draws nothing");
    }
}
