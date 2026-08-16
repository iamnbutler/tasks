//! A single-line text field.
//!
//! **A bare `input()` lays out at zero height, and that is the whole reason
//! this exists.** gpuikit's element forces `relative(1.)` on both axes only in
//! multiline mode; a single-line one passes its style through untouched, and it
//! is a childless leaf, so gpui hands it to taffy as `new_leaf` with
//! `height: auto` — which resolves to 0px. The field then paints nothing (its
//! content mask is empty), shows no caret, and registers a hitbox with no area,
//! so it cannot be clicked either. It is still focusable and still takes
//! keystrokes, which is what makes the bug read as "the window is inert" rather
//! than as a missing element: the text is going somewhere, just nowhere
//! visible. Both single-line fields in this app shipped that way — the Add Repo
//! window and the palettes.
//!
//! So the height is not a style choice, it is the thing that makes the element
//! exist, and it lives here rather than at each call site so the next field
//! does not have to rediscover it. Chrome — border, background, padding — is
//! deliberately *not* here: the palette's field sits in a bordered header and a
//! box around it would be a second box, while the Add Repo window wants one.
//! Height and placeholder are what every caller needs; the rest is theirs.
//!
//! **Focus is the caller's too, and it takes both halves.** A surface holding
//! one of these focuses it when it opens, *and* focuses it again on a mouse
//! down anywhere in its container — the padding, the label, the empty half of
//! a row. Without the second half a click that misses the field by a few
//! pixels leaves the keyboard pointed at nothing, which reads as a surface
//! that has stopped responding rather than as a missed target. It is safe to
//! add because gpuikit raises `InputStateEvent::Blur` from the input's
//! *paint*, so a click that moves focus nowhere raises no blur — and a blur is
//! what closes the Add Repo window.

use gpui::{div, px, App, Div, Entity, ParentElement, Pixels, SharedString, Styled};
use gpuikit::elements::input::input;
use gpuikit::input::InputState;

/// One line of text at the app's `text_sm`, with room for a descender.
const FIELD_HEIGHT: Pixels = px(24.);

/// A single-line input at a definite height, filling the width it is given.
pub fn text_field(
    state: &Entity<InputState>,
    placeholder: impl Into<SharedString>,
    cx: &App,
) -> Div {
    div()
        .flex()
        .items_center()
        .h(FIELD_HEIGHT)
        .w_full()
        .child(input(state, cx).placeholder(placeholder).size_full())
}
