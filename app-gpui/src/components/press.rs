//! Swallowing a press, for controls that float over selectable text.
//!
//! gpuikit's selectable markdown anchors a text selection from a
//! **bubble-phase `MouseDownEvent`** (`gpuikit/src/markdown/selectable_text.rs`).
//! So a floating control painted over a markdown document — the copy button on
//! an orchestrator message, the jump-to-newest pill, the chat chip, a modal
//! scrim — starts a selection in the text underneath unless something stops
//! that event before it reaches the document.
//!
//! Three things about the fix are easy to get wrong, and none of them is
//! recoverable from a call site:
//!
//! * **It has to be on mouse *down*, not in `on_click`.** A click resolves on
//!   the mouse *up* that follows, by which point the drag has been live for the
//!   whole gesture. `modal.rs`'s panel guard is the prior art for this idiom
//!   and says the same thing in its own words.
//! * **It has to sit on an *ancestor* of the control, not beside it.** gpui
//!   runs bubble listeners in reverse registration order and
//!   `Interactivity::paint` registers an element's listeners *before* painting
//!   its children, so an ancestor's listener runs after every listener its
//!   descendants registered — including the bubble-phase mouse-down that stores
//!   `pending_mouse_down`, which is what `on_click` consumes on mouse-up. Put
//!   the same guard on a preceding sibling, or ahead of the control's own
//!   listeners, and the control silently stops clicking.
//! * **It is not `.occlude()` / `.block_mouse_except_scroll()`.** Those block
//!   the mouse from *elements behind* this one, and they do stop the drag — by
//!   making every hitbox behind them read un-hovered. The element whose hover
//!   *reveals* a hover-revealed affordance is one of those: `group_hover`
//!   resolves through the group element's hitbox, the group is an ancestor of
//!   the row, and an ancestor's hitbox is behind its descendant's. The
//!   affordances would fade to `opacity(0.)` the moment the pointer reached
//!   them, while still being clickable — and `occlude()` additionally swallows
//!   the scroll wheel over an invisible strip in every message's top-right
//!   corner. Stopping one event leaves hit testing untouched, which is why it
//!   is the right-sized tool.
//!
//! Only the left button, and only the press. Mouse *up* still propagates, which
//! matters: a selection dragged from elsewhere and released over an overlay
//! must still end its drag. A right-press still reaches the markdown, which
//! does nothing with it today, so a context menu added there later is not
//! silently eaten.
//!
//! **Why not gpuikit's `IconButton`.** The obvious home for this is the shared
//! `icon_button` helper, and it is not ours: `gpuikit::elements::icon_button`
//! is a git dependency. It already carries `cx.stop_propagation()` inside its
//! `on_click` (too late) and `window.prevent_default()` on its mouse-down (the
//! wrong verb). Moving a `stop_propagation` onto that mouse-down is the correct
//! *upstream* change; if it ever lands, the call sites here become redundant
//! but stay harmless.

use gpui::{InteractiveElement, MouseButton};

/// Stop a left mouse-down from reaching whatever this element floats over.
///
/// Apply it to the floating *container*, which is an ancestor of the
/// interactive child — see the module docs for why that is what keeps the
/// click working.
pub trait SwallowPress: InteractiveElement + Sized {
    fn swallow_press(self) -> Self {
        self.on_mouse_down(MouseButton::Left, |_event, _window, cx| {
            cx.stop_propagation();
        })
    }
}

impl<E: InteractiveElement + Sized> SwallowPress for E {}
