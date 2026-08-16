//! Drag-to-reorder for list rows: one row that is both a drag source and a
//! drop target, and the pure list move behind it.
//!
//! Deliberately self-contained — it imports `gpui` and `gpuikit::theme` and
//! knows nothing about tasks, specs or ranks. gpuikit has no sortable of its
//! own (#866); if it grows one, this file is deleted and re-exported from
//! there rather than rewritten, and no caller changes shape.
//!
//! Two things about gpui's drag primitives are worth knowing before reading
//! [`sortable`]:
//!
//! - **Drags are matched by `TypeId`.** The payload type *is* the list a row
//!   belongs to, so two orderings sharing one screen cannot swallow each
//!   other's rows. Anything finer than "which list" — which band, which row —
//!   is the `accepts` predicate's job.
//! - **`can_drop` does not gate `drag_over`.** They are separate mechanisms: a
//!   row filtered by `can_drop` alone still lights up as a target and then
//!   refuses the drop. So `accepts` is consulted in both places, which is
//!   possible because the `drag_over` closure is handed the dragged value.

use std::rc::Rc;

use gpui::prelude::*;
use gpui::{div, px, App, Context, Div, Pixels, Point, SharedString, Stateful, Window};
use gpuikit::theme::{ActiveTheme, Themeable};

/// Height of the chip in [`DragPreview`]. A constant because the chip centres
/// itself on the pointer, and nothing has laid it out yet when it does.
const PREVIEW_HEIGHT: f32 = 24.;

/// The chip that follows the pointer while a row is in flight.
pub struct DragPreview {
    label: SharedString,
    /// Where inside the row the drag started. gpui prepaints this view at
    /// `mouse − cursor_offset` — the row's own origin — so the chip offsets
    /// itself by the grab point to sit back under the pointer.
    grab: Point<Pixels>,
}

impl Render for DragPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        div()
            .pl(self.grab.x + px(12.))
            .pt(self.grab.y - px(PREVIEW_HEIGHT / 2.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .h(px(PREVIEW_HEIGHT))
                    .px(px(8.))
                    .rounded(px(5.))
                    .bg(theme.surface_tertiary())
                    .border_1()
                    .border_color(theme.border_subtle())
                    .shadow_md()
                    .text_xs()
                    .text_color(theme.fg())
                    .child(self.label.clone()),
            )
    }
}

/// Decorate `row` as a drag source carrying `payload`, and as a drop target
/// for payloads of the same type.
///
/// `accepts` says whether a given dragged payload may land here — it is what
/// keeps a row out of a band it does not belong to, and what stops a row from
/// being dropped on itself. `on_drop` runs only for payloads it accepted.
///
/// Returns the decorated row, so a caller is free to wrap it further (a
/// context menu, say) exactly as it would have wrapped the undecorated one.
pub fn sortable<P: 'static>(
    row: Stateful<Div>,
    payload: P,
    preview: impl Into<SharedString>,
    accepts: impl Fn(&P) -> bool + 'static,
    on_drop: impl Fn(&P, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    // One predicate, two consumers: the highlight and the drop must agree, or
    // the row promises a landing it then refuses.
    let accepts = Rc::new(accepts);
    let label = preview.into();

    row.cursor_grab()
        .on_drag(payload, move |_payload, grab, _window, cx| {
            let label = label.clone();
            cx.new(move |_| DragPreview { label, grab })
        })
        .drag_over::<P>({
            let accepts = accepts.clone();
            move |style, dragged: &P, _window, cx| {
                if accepts(dragged) {
                    style.bg(cx.theme().accent_bg())
                } else {
                    style
                }
            }
        })
        .on_drop(move |dragged: &P, window, cx| {
            if accepts(dragged) {
                on_drop(dragged, window, cx);
            }
        })
}

/// The reorder itself: `order` with `moved` taken out and put back where
/// `target` sits.
///
/// `None` when the drop changes nothing — dropped on itself, or either id
/// missing from the list — so a caller can skip the round trip rather than
/// posting the order it already has.
pub fn move_to<T: Clone + PartialEq>(order: &[T], moved: &T, target: &T) -> Option<Vec<T>> {
    let from = order.iter().position(|item| item == moved)?;
    let to = order.iter().position(|item| item == target)?;
    if from == to {
        return None;
    }
    let mut next = order.to_vec();
    let item = next.remove(from);
    next.insert(to, item);
    Some(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moving_down_lands_on_the_targets_place() {
        let order = ["a", "b", "c", "d"];
        assert_eq!(move_to(&order, &"a", &"c").unwrap(), ["b", "c", "a", "d"]);
    }

    #[test]
    fn moving_up_lands_on_the_targets_place() {
        let order = ["a", "b", "c", "d"];
        assert_eq!(move_to(&order, &"d", &"b").unwrap(), ["a", "d", "b", "c"]);
    }

    /// Everything the drag did not touch keeps its relative order — which is
    /// what makes posting the whole list safe.
    #[test]
    fn nothing_else_moves_relative_to_anything_else() {
        let order = ["a", "b", "c", "d", "e"];
        let next = move_to(&order, &"e", &"a").unwrap();
        assert_eq!(next, ["e", "a", "b", "c", "d"]);
    }

    #[test]
    fn a_row_dropped_on_itself_is_not_a_reorder() {
        let order = ["a", "b"];
        assert!(move_to(&order, &"a", &"a").is_none());
    }

    /// A stale row — dragged from a list the server has since changed under
    /// us. Nothing to post: the id is not there to move.
    #[test]
    fn an_id_that_is_not_in_the_list_is_not_a_reorder() {
        let order = ["a", "b"];
        assert!(move_to(&order, &"z", &"a").is_none());
        assert!(move_to(&order, &"a", &"z").is_none());
    }
}
