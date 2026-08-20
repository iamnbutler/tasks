//! The charter, visible and editable — the nine capabilities as rows, with
//! Off / Shadow / Live per row (#993).
//!
//! Before this, the charter existed only as rows in SQLite and a generated
//! section of the orchestrator's system prompt. It is the one thing that
//! decides whether this system merges its own pull requests and closes your
//! issues, and the answer to "how do I stop it doing that" was a `curl`.
//!
//! It observes [`AppState`], so a charter written from anywhere else — the
//! API, another client — lands here without this window asking.

use gpui::prelude::*;
use gpui::{
    div, px, size, App, Bounds, Context, Entity, Global, Render, SharedString, TitlebarOptions,
    Window, WindowBounds, WindowHandle, WindowOptions,
};
use gpuikit::elements::tooltip::tooltip;
use gpuikit::theme::{ActiveTheme, Themeable};
use tasks_client::api::models::{Capability, CharterEntry, CharterLevel};

use crate::state::AppState;

/// What each level means, in the person's terms.
///
/// `shadow` gets the longest, being the only counter-intuitive one: every
/// other UI in the world reads a middle setting as "sometimes", and this one
/// means "always decides, never acts".
pub fn level_meaning(level: CharterLevel) -> &'static str {
    match level {
        CharterLevel::Off => "Refused. It cannot do this, and it is told so.",
        CharterLevel::Shadow => {
            "It decides as it would and the decision is recorded, and then nothing happens. \
             For a capability you have taken away but whose reasoning you still want to read."
        }
        CharterLevel::Live => "It does this on its own, without asking you.",
    }
}

/// Singleton, like the About window: a second Charter… raises the one open.
struct CharterWindowHandle(WindowHandle<CharterWindow>);

impl Global for CharterWindowHandle {}

pub fn open(cx: &mut App) {
    if let Some(existing) = cx
        .try_global::<CharterWindowHandle>()
        .map(|global| global.0)
    {
        if existing
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
        {
            cx.activate(true);
            return;
        }
    }
    let Some(app_state) = crate::state::global(cx) else {
        return;
    };

    let options = WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some("Charter".into()),
            appears_transparent: false,
            traffic_light_position: None,
        }),
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(620.), px(640.)),
            cx,
        ))),
        ..Default::default()
    };

    match cx.open_window(options, |_window, cx| {
        cx.new(|cx| CharterWindow::new(app_state, cx))
    }) {
        Ok(handle) => {
            cx.set_global(CharterWindowHandle(handle));
            cx.activate(true);
        }
        Err(error) => eprintln!("failed to open the Charter window: {error}"),
    }
}

pub struct CharterWindow {
    app_state: Entity<AppState>,
}

impl CharterWindow {
    fn new(app_state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        // A charter written elsewhere lands here.
        cx.observe(&app_state, |_this, _state, cx| cx.notify())
            .detach();
        Self { app_state }
    }

    fn row(&self, entry: &CharterEntry, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let capability = entry.capability;
        let level = entry.level;

        let chip = |target: CharterLevel| {
            let selected = target == level;
            div()
                .id(SharedString::from(format!(
                    "{}-{}",
                    capability.as_str(),
                    target.as_str()
                )))
                .px(px(8.))
                .py(px(2.))
                .rounded(px(5.))
                .border_1()
                .border_color(theme.border_secondary())
                .text_xs()
                .text_color(match selected {
                    true => theme.fg(),
                    false => theme.fg_muted(),
                })
                .when(selected, |el| el.bg(theme.surface_tertiary()))
                .cursor_pointer()
                .child(crate::components::title_case(target.as_str()))
                .tooltip(tooltip(level_meaning(target)))
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.app_state
                        .update(cx, |state, cx| state.set_charter(capability, target, cx));
                }))
        };

        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .py(px(8.))
            .border_b_1()
            .border_color(theme.border_secondary())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.fg())
                            .child(capability.title()),
                    )
                    .child(div().flex_1())
                    .child(chip(CharterLevel::Off))
                    .child(chip(CharterLevel::Shadow))
                    .child(chip(CharterLevel::Live)),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.fg_muted())
                    .child(capability.consequence()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.fg_muted())
                    .child(level_meaning(level)),
            )
            .into_any_element()
    }
}

impl Render for CharterWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let charter = self.app_state.read(cx).charter.clone();
        // `BY_CONSEQUENCE`, so the row that merges pull requests is the first
        // one read — not `ALL`, which is the order the charter is *flipped*
        // in and puts editing issues at the top of a list nobody reads that
        // way.
        let rows: Vec<gpui::AnyElement> = Capability::BY_CONSEQUENCE
            .iter()
            .map(|capability| {
                let entry = charter
                    .iter()
                    .find(|entry| entry.capability == *capability)
                    .cloned()
                    .unwrap_or(CharterEntry {
                        capability: *capability,
                        // A missing row is `off` at the server, so it is `off`
                        // here. Silence must not read as permission.
                        level: CharterLevel::Off,
                        daily_limit: None,
                        updated_at: chrono::DateTime::UNIX_EPOCH,
                    });
                self.row(&entry, cx)
            })
            .collect();

        div()
            .flex()
            .flex_col()
            .size_full()
            .p(px(20.))
            .gap(px(4.))
            .bg(theme.bg())
            .font_family(crate::workspace::FONT)
            .child(
                div()
                    .text_color(theme.fg())
                    .child("What Tasks may do on its own"),
            )
            .child(div().text_xs().text_color(theme.fg_muted()).child(
                "Each of these is enforced at the server, not by the agent's own judgment. \
                 Changes take effect on the next action.",
            ))
            .child(
                div()
                    .id("charter-rows")
                    .mt(px(8.))
                    .flex_1()
                    .overflow_y_scroll()
                    .children(rows),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three levels, three distinct explanations. An empty one renders as a
    /// blank line under a row rather than failing to compile.
    #[test]
    fn every_level_explains_itself() {
        let meanings: Vec<&str> = [CharterLevel::Off, CharterLevel::Shadow, CharterLevel::Live]
            .iter()
            .map(|level| level_meaning(*level))
            .collect();
        for meaning in &meanings {
            assert!(!meaning.trim().is_empty());
        }
        assert_eq!(
            meanings
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            3
        );
    }

    /// Shadow is the counter-intuitive one — every other middle setting in
    /// software means "sometimes", and this one means "always decides, never
    /// acts". It gets the longest explanation for that reason, and a shorter
    /// one here is a sign it has been trimmed into ambiguity.
    #[test]
    fn shadow_gets_the_longest_explanation() {
        let shadow = level_meaning(CharterLevel::Shadow).len();
        assert!(shadow > level_meaning(CharterLevel::Off).len());
        assert!(shadow > level_meaning(CharterLevel::Live).len());
    }
}
