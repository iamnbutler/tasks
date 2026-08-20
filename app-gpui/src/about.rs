//! The About window — the app's name, the build it came from, and what this
//! thing does when you turn it on.
//!
//! Same information set the Swift about panel showed: a marketing version and
//! a monospaced commit line. Both are stamped by `build.rs`; `0.1.0` with
//! `commit unknown` means the binary was built without git in reach.
//!
//! Beneath them, [`crate::disclaimer`] and one link to the repository. The
//! disclaimer already sends the reader to "README.md, under Read this first",
//! and this is the one window in the app with no checkout behind it — so that
//! pointer named a file the reader had nothing to open. One link and not a row
//! of them: a footer of links is the marketing register the paragraph below
//! rules out.
//!
//! **The icon half of #998 is deliberately not done.** There is no icon in the
//! repository — no `.icns`, no `CFBundleIconFile` in `Info.plist.in`, nothing
//! copied by `app-install` — and drawing a placeholder mark would be the
//! microphone one window over. When a real icon lands it goes above the name
//! here, and the bundle wants it at the same time.
//!
//! This is the window a stranger opens
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

/// `0.1.<commit count>`, or the crate version with no git in reach.
pub const VERSION: &str = env!("TASKS_GPUI_VERSION");
/// Short SHA (`-dirty` when the tree had uncommitted changes), or `unknown`.
pub const COMMIT: &str = env!("TASKS_GPUI_COMMIT");
/// Where the source lives — what [`crate::disclaimer::README_POINTER`] sends
/// the reader to, made openable.
pub const REPOSITORY: &str = "https://github.com/iamnbutler/tasks";

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
            // 330 rather than 300 for the repository row: `is_resizable` is
            // false, so this will not self-correct.
            size(px(380.), px(330.)),
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
            .child(
                div()
                    .id("about-repository")
                    .mt(px(4.))
                    .text_xs()
                    .text_color(theme.fg_muted())
                    .cursor_pointer()
                    .hover({
                        let fg = theme.fg();
                        move |el| el.text_color(fg)
                    })
                    .on_click(|_event, _window, cx| cx.open_url(REPOSITORY))
                    .child(REPOSITORY),
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

    /// The link has to be openable, which means a real absolute GitHub URL —
    /// and no trailing slash, because it is rendered as the label too.
    #[test]
    fn the_repository_link_is_a_real_url() {
        assert!(
            REPOSITORY.starts_with("https://github.com/"),
            "{REPOSITORY}"
        );
        assert!(!REPOSITORY.ends_with('/'), "{REPOSITORY}");
        assert_eq!(REPOSITORY.matches('/').count(), 4, "owner/repo, no deeper");
    }

    /// The link exists to serve the disclaimer's pointer. If that sentence
    /// ever stops naming the README, this row's reason for being here has
    /// changed and somebody should say so on purpose.
    #[test]
    fn the_pointer_this_link_serves_still_names_the_readme() {
        assert!(
            disclaimer::README_POINTER.contains("README.md"),
            "{}",
            disclaimer::README_POINTER
        );
    }
}
