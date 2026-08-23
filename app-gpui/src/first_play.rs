//! What pressing `play` will do, said once per install (#993).
//!
//! The rule-bearing half is [`tasks_api::first_play`], where `make test`
//! actually runs it — `app-gpui` is not a workspace member. What is here is
//! chrome: the process-wide answer to "has this been acknowledged", and the
//! sheet's body as an element.
//!
//! **One [`Global`], not a field per window.** Both app windows can start the
//! pipeline and the menubar can ask about it, so acknowledging in the Server
//! window must not leave the Workspace about to ask again. It is seeded from
//! disk at startup and set by whichever window's sheet is confirmed.
//!
//! **A failed write is not a refusal.** No `$HOME`, a read-only data dir: the
//! pipeline still starts and the global remembers for the session. A sheet
//! that cannot be dismissed permanently is exactly the trained-out-of-use
//! surface the issue argues against.

use gpui::prelude::*;
use gpui::{div, px, App, IntoElement, SharedString};
use gpuikit::theme::{ActiveTheme, Themeable};
use tasks_api::first_play::{Sheet, SheetLine, UNREADABLE_CHARTER};
use tasks_api::models::CharterEntry;

/// This modal's seat in a window's [`crate::modal::ModalLayer`].
pub const SHEET_MODAL: &str = "first-play";

pub use crate::server::FirstPlay;

/// The sheet's title.
pub const TITLE: &str = "Starting the pipeline";

/// The last paragraph: where the off switches are.
pub const OFF_SWITCHES: &str = "You can stop it at any time: Pause or Stop in this same row, \
     any capability to off in the charter below, or \"Kill All Containers\" in the command \
     palette.";

/// The sheet's body: the fixed caution, then the charter as three groups.
///
/// `charter` is what the client actually fetched — `None` means the fetch
/// failed or the server predates the route, which renders as its own state and
/// **never** as three empty groups. Three empty groups would read as "it will
/// do nothing", on the one surface that exists to warn.
pub fn sheet_body(charter: Option<&[CharterEntry]>, cx: &App) -> impl IntoElement {
    let theme = cx.theme().clone();
    let sheet = Sheet::from_charter(charter);

    let mut body = div()
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(
            div()
                .text_sm()
                .text_color(theme.fg())
                .child(crate::disclaimer::PIPELINE_CAUTION),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.fg_muted())
                .child(crate::disclaimer::README_POINTER),
        );

    if sheet.unreadable {
        body = body.child(
            div()
                .text_xs()
                .text_color(theme.warning())
                .child(UNREADABLE_CHARTER),
        );
    } else {
        body = body
            .children(group("It will, without asking:", &sheet.live, cx))
            .children(group("It will decide but not act (shadow):", &sheet.shadow, cx))
            .children(group("It will not:", &sheet.off, cx));
    }

    body.child(
        div()
            .text_xs()
            .text_color(theme.fg_muted())
            .child(OFF_SWITCHES),
    )
}

/// One group, or nothing when it is empty — an empty heading is a row a reader
/// learns to skip.
fn group(heading: &'static str, lines: &[SheetLine], cx: &App) -> Option<gpui::AnyElement> {
    if lines.is_empty() {
        return None;
    }
    let theme = cx.theme().clone();
    Some(
        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(div().text_xs().text_color(theme.fg()).child(heading))
            .children(lines.iter().map(|line| {
                div()
                    .text_xs()
                    .text_color(theme.fg_muted())
                    .child(SharedString::from(format!("• {}", line.permits)))
            }))
            .into_any_element(),
    )
}
