//! One modal layer: a scrim, a focus contract, and **one** dismissal
//! predicate — for every surface in this app that has to be answered or put
//! away before anything behind it can be touched.
//!
//! It exists because the app had grown two hand-rolled overlays that shared
//! nothing (the command palette and the Server window's Stop confirmation),
//! and three more were queued behind them (#992's install call-to-action,
//! #993's before-first-`play` sheet, #1005's secrets fields). Both existing
//! ones are ported onto this, which is the only evidence that it generalises;
//! a third hand-rolled overlay is the thing this module exists to make
//! unnecessary.
//!
//! Five semantics are the component, and each is a way the hand-rolled ones
//! differed from each other.
//!
//! - **One at a time.** [`ModalLayer`] holds at most one modal. A request for
//!   a *different* one while a modal is up is a [`ModalConflict`] the caller
//!   surfaces — never a stack to manage, because a stack is a focus-restore
//!   order and a dismissal order nobody has designed. Re-requesting the one
//!   that is already up is a no-op on purpose: the two palettes switch between
//!   themselves under one modal, and that is a content change rather than a
//!   second surface.
//! - **Focus is captured at open and restored on dismiss.** Whatever had
//!   focus when the modal went up gets it back — the invoking element, not
//!   some window-wide default. It is captured on the *first* open and survives
//!   a re-request, so switching palettes cannot make the palette's own query
//!   field the thing focus is restored to.
//! - **Focus is held while it is open.** The scrim occludes every pointer
//!   target behind the modal, and [`ModalLayer::hold_focus`] — called once per
//!   frame from the host's render — pulls focus back if anything else took it.
//!   That is the honest extent of the trap: gpui has a tab order, so "nothing
//!   can be clicked" is not enough on its own.
//! - **Escape and the scrim are one predicate, not two.** [`Dismissal`] is a
//!   single field read by both, so a modal that Escape cannot close cannot be
//!   clicked away either. gpuikit's own `DialogState` has two independent
//!   booleans here, which is exactly the drift being avoided.
//! - **⌘-Enter is the default answer**, matching every composer in the app
//!   (`SubmitOn::CmdEnter`), and plain Enter never confirms a modal from a
//!   text field.
//!
//! It lives here rather than in gpuikit on purpose. gpuikit is the natural
//! long-term home and already ships a `DialogState`, but it is a rev-pinned
//! git dependency, so upstreaming means a PR there and a rev bump here —
//! worth paying once the shape has survived the consumers queued behind the
//! first two, not before.
//!
//! The key bindings carry that last pair, and the split between them is
//! load-bearing. Escape is bound in `Modal` **and** in `"Modal > Input"`, so
//! it dismisses the modal even with a field focused: inside a modal, escape
//! means "close this", and blurring the field instead is what left the palette
//! depending on a blur event to close itself. ⌘-Enter is bound in `Modal`
//! **only**, deliberately: an input with `SubmitOn::CmdEnter` inside a modal
//! keeps its own binding (gpuikit's `Input` context is one level deeper and
//! wins), so a form's field submits the form, and the host wires that
//! `Submit` event to the same action ⌘-Enter reaches when no field is
//! focused. Binding it in `"Modal > Input"` too would tie on depth and steal
//! it back — see [`bind_keys`].

use std::fmt;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    actions, div, px, AnyElement, App, Context, Div, FocusHandle, IntoElement, KeyBinding,
    MouseButton, Pixels, SharedString, Window,
};
use gpuikit::theme::{ActiveTheme, Themeable};

actions!(
    modal,
    [
        /// Escape: put the modal away — where [`Dismissal`] allows it.
        DismissModal,
        /// ⌘-Enter: the modal's default answer.
        SubmitModal
    ]
);

/// The keymap context the modal panel sets on itself.
pub const MODAL_CONTEXT: &str = "Modal";

/// The predicate escape is *also* bound under: a text field inside a modal.
/// Two spellings of one fact with [`MODAL_CONTEXT`], pinned by a test.
const MODAL_INPUT: &str = "Modal > Input";

/// Bind escape and ⌘-Enter.
///
/// **Must run after `gpuikit::input::bind_input_keys`.** The `"Modal > Input"`
/// escape ties with gpuikit's own `Input` escape on context depth
/// (`KeyBindingContextPredicate::depth_of` reports `A > B` at `B`'s depth),
/// and gpui breaks a depth tie on registration order, later wins. Registered
/// first, escape inside a modal's field goes back to blurring the field and
/// the modal stays up. This is the same tie `palette::bind_keys` turns, for
/// the same reason.
///
/// ⌘-Enter is bound in the bare [`MODAL_CONTEXT`] and nowhere else: see the
/// module docs. A field inside a modal must keep its own ⌘-Enter.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", DismissModal, Some(MODAL_CONTEXT)),
        KeyBinding::new("escape", DismissModal, Some(MODAL_INPUT)),
        KeyBinding::new("cmd-enter", SubmitModal, Some(MODAL_CONTEXT)),
    ]);
}

/// Whether this modal can be put away without answering it.
///
/// One value, read by escape *and* by the scrim, because two flags is how a
/// surface ends up clickable-away but not escapable — the state where the
/// gesture a user reaches for depends on which one the author remembered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dismissal {
    /// Escape and a click outside both put it away. The default: most modals
    /// are a question whose answer may be "not now".
    Dismissible,
    /// Neither does. For a modal whose only exits are its own buttons —
    /// a destructive confirm where "I clicked next to it" must not count as
    /// an answer.
    ///
    /// Nothing ships on this yet — the Stop confirmation deliberately does
    /// not, because Cancel is a real answer there and a modal whose *safe*
    /// exit needs a specific button is one whose destructive button is the
    /// easier target. It exists because "escape does not always dismiss" is
    /// half of what makes the predicate a predicate: written as an
    /// `on_dismiss` a consumer can forget to pass, an undismissable modal
    /// would be an omission rather than a decision.
    #[allow(dead_code)]
    MustAnswer,
}

impl Dismissal {
    /// The one predicate. Both escape and the scrim ask this and nothing else.
    pub fn dismissible(self) -> bool {
        matches!(self, Dismissal::Dismissible)
    }
}

/// What the layer draws behind the panel.
///
/// Both variants catch every click; the difference is only whether you can
/// read what is underneath. A dim says "this is the only thing to answer"; a
/// clear one is for a surface that is *about* what is behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scrim {
    /// Dimmed — the theme's overlay.
    Dim,
    /// Transparent. A click catcher, not a scrim: the palette needs this,
    /// because a refused row writes its reason into the sidebar banner behind
    /// the panel, and a dim would hide the answer it just gave.
    Clear,
}

/// Where the panel sits in the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Centred — a question, which is most of them.
    Center,
    /// Pinned this far from the top. For a surface you type into: a centred
    /// panel that grows downward as it matches moves the field under the
    /// caret, which the palette must not do.
    Top(Pixels),
}

/// What escape, the scrim and ⌘-Enter each run. Bare callbacks rather than
/// event listeners: the same answer has to be reachable from a key binding and
/// from a mouse press, and two listeners is two places to forget the
/// [`ModalLayer::dismiss`] that hands focus back.
type Answer = Rc<dyn Fn(&mut Window, &mut App)>;

/// A modal was requested while a different one was open.
///
/// Returned rather than resolved, because there is no answer here that is not
/// a guess: dropping the request loses work, replacing the open modal loses an
/// answer someone is mid-way through giving. The caller says it out loud —
/// this is a bug in the code that asked, and it is one keystroke away from
/// being invisible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModalConflict {
    /// The modal that is already up.
    pub open: &'static str,
    /// The one that was asked for.
    pub requested: &'static str,
}

impl fmt::Display for ModalConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the {} dialog is already open — {} cannot open over it",
            self.open, self.requested
        )
    }
}

/// What [`ModalLayer::open`] is about to do, decided before any focus moves.
///
/// Split out so the rule is a pure function with a test, rather than something
/// only reachable through a live `Window`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transition {
    /// Nothing was open: capture focus and raise it.
    Raise,
    /// This same modal is already up. Idempotent — the capture is *not*
    /// redone, so the restore target stays whatever was focused before the
    /// modal first appeared.
    AlreadyOpen,
}

/// The rule, pure: one modal at a time, re-requesting the open one is a no-op,
/// and anything else is the caller's bug.
fn resolve(
    open: Option<&'static str>,
    requested: &'static str,
) -> Result<Transition, ModalConflict> {
    match open {
        None => Ok(Transition::Raise),
        Some(open) if open == requested => Ok(Transition::AlreadyOpen),
        Some(open) => Err(ModalConflict { open, requested }),
    }
}

struct OpenModal {
    id: &'static str,
    /// The panel's own handle. What the key context hangs off, and what
    /// [`ModalLayer::hold_focus`] measures "focus is still inside" against.
    focus: FocusHandle,
    /// Where focus goes on open, and back to on a click on the panel's own
    /// chrome — the query field, for a modal that has one; the panel itself
    /// otherwise.
    target: FocusHandle,
    /// Where focus goes when this closes: whatever had it when the modal went
    /// up, or the host's own handle if nothing did.
    restore: FocusHandle,
}

/// The host's modal state: at most one open modal, plus the focus it owes
/// back.
///
/// A plain struct on the view rather than an `Entity`, like `PaletteState`:
/// there is nothing here to observe that the host does not already notify for,
/// and a second entity would be a second thing that can disagree about whether
/// a modal is up.
pub struct ModalLayer {
    /// Where focus is restored to when nothing held it at open time. The host
    /// view's own handle: gpui falls back to a root dispatch node that carries
    /// no key context, so leaving focus nowhere would take the window's
    /// shortcuts with it.
    fallback: FocusHandle,
    open: Option<OpenModal>,
}

impl ModalLayer {
    pub fn new(fallback: FocusHandle) -> Self {
        Self {
            fallback,
            open: None,
        }
    }

    /// Raise `id`, capturing the focus it will owe back.
    ///
    /// `target` is what to focus while it is open — a query field, say. `None`
    /// focuses the panel itself, which is what makes escape and ⌘-Enter reach
    /// a modal with nothing to type in.
    ///
    /// Idempotent for the modal that is already up: focus is moved to `target`
    /// again (⌘P over an open ⌘⇧P should still put the caret in the field),
    /// but the restore target is *not* recaptured — recapturing it there is
    /// how a modal ends up restoring focus to its own field.
    pub fn open(
        &mut self,
        id: &'static str,
        target: Option<FocusHandle>,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<(), ModalConflict> {
        match resolve(self.open.as_ref().map(|open| open.id), id)? {
            Transition::Raise => {
                let focus = cx.focus_handle();
                let restore = window.focused(cx).unwrap_or_else(|| self.fallback.clone());
                let target = target.unwrap_or_else(|| focus.clone());
                window.focus(&target, cx);
                self.open = Some(OpenModal {
                    id,
                    focus,
                    target,
                    restore,
                });
            }
            Transition::AlreadyOpen => {
                if let Some(open) = self.open.as_mut() {
                    if let Some(target) = target {
                        open.target = target;
                    }
                    let target = open.target.clone();
                    window.focus(&target, cx);
                }
            }
        }
        Ok(())
    }

    /// Put whatever is open away and hand focus back to the element that
    /// raised it. Returns whether there was anything to close, so a host can
    /// tell a dismissal from a keystroke that hit nothing.
    pub fn dismiss(&mut self, window: &mut Window, cx: &mut App) -> bool {
        let Some(open) = self.open.take() else {
            return false;
        };
        window.focus(&open.restore, cx);
        true
    }

    /// Whether `id` is the modal that is up.
    pub fn is_open(&self, id: &'static str) -> bool {
        self.open.as_ref().is_some_and(|open| open.id == id)
    }

    /// The other half of the trap, called once per frame from the host's
    /// render: if focus has left the modal while it is open, take it back.
    ///
    /// The scrim stops the pointer; this stops everything else — gpui carries
    /// a tab order, and an unfocused modal is one whose escape key does
    /// nothing, which reads as a hung window rather than as a focus bug.
    pub fn hold_focus(&self, window: &mut Window, cx: &mut App) {
        let Some(open) = self.open.as_ref() else {
            return;
        };
        if !open.focus.contains_focused(window, cx) {
            let target = open.target.clone();
            window.focus(&target, cx);
        }
    }
}

/// The open modal as an element, or `None` when nothing is up.
///
/// Takes the layer rather than an id so the panel's focus handles come from
/// the one place that owns them: an element built with handles of its own
/// would be a second answer to "what is focused", and the first one to drift.
pub fn modal(layer: &ModalLayer) -> Option<Modal> {
    let open = layer.open.as_ref()?;
    Some(Modal {
        id: open.id,
        focus: open.focus.clone(),
        target: open.target.clone(),
        scrim: Scrim::Dim,
        dismissal: Dismissal::Dismissible,
        placement: Placement::Center,
        on_dismiss: None,
        on_submit: None,
        child: None,
    })
}

/// The chrome a modal's content sits in: surface, border, rounding, shadow.
///
/// Separate from [`modal`] because look and behaviour are separable — the
/// palette wants this box at a fixed width with its own header and footer
/// rules inside it, and a wrapper that imposed padding would fight all three.
pub fn panel(cx: &App) -> Div {
    let theme = cx.theme();
    div()
        .flex()
        .flex_col()
        .rounded(px(8.))
        .bg(theme.surface())
        .border_1()
        .border_color(theme.border())
        .shadow_lg()
}

/// The open modal, as an element. Build it with [`modal`].
#[derive(IntoElement)]
pub struct Modal {
    id: &'static str,
    focus: FocusHandle,
    target: FocusHandle,
    scrim: Scrim,
    dismissal: Dismissal,
    placement: Placement,
    on_dismiss: Option<Answer>,
    on_submit: Option<Answer>,
    child: Option<AnyElement>,
}

impl Modal {
    pub fn scrim(mut self, scrim: Scrim) -> Self {
        self.scrim = scrim;
        self
    }

    pub fn dismissal(mut self, dismissal: Dismissal) -> Self {
        self.dismissal = dismissal;
        self
    }

    pub fn placement(mut self, placement: Placement) -> Self {
        self.placement = placement;
        self
    }

    /// What escape and a click outside do — where [`Dismissal`] allows them.
    /// One handler for both, which is the predicate having one consequence as
    /// well as one condition.
    pub fn on_dismiss<V: 'static>(
        mut self,
        cx: &Context<V>,
        handler: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    ) -> Self {
        self.on_dismiss = Some(view_callback(cx, handler));
        self
    }

    /// What ⌘-Enter does: the modal's default answer, and never the
    /// destructive one.
    pub fn on_submit<V: 'static>(
        mut self,
        cx: &Context<V>,
        handler: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    ) -> Self {
        self.on_submit = Some(view_callback(cx, handler));
        self
    }

    pub fn child(mut self, child: impl IntoElement) -> Self {
        self.child = Some(child.into_any_element());
        self
    }
}

/// Wrap a view method as a bare callback the element can hold two copies of.
fn view_callback<V: 'static>(
    cx: &Context<V>,
    handler: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
) -> Answer {
    let entity = cx.entity();
    Rc::new(move |window, cx| {
        entity.update(cx, |view, cx| handler(view, window, cx));
    })
}

impl RenderOnce for Modal {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme().clone();
        let dismissible = self.dismissal.dismissible();
        let target = self.target.clone();

        // The panel, first: it carries the key context, holds the focus, and
        // occludes the scrim's hitbox.
        let mut panel = div()
            .id(SharedString::from(format!("modal:{}", self.id)))
            .key_context(MODAL_CONTEXT)
            .track_focus(&self.focus)
            // A press on the panel is never a press on the scrim. This is what
            // replaces the palette's old backdrop-and-panel-as-siblings
            // arrangement: gpui delivers to the topmost element and bubbles
            // through ancestors, so the panel runs first and stops it here.
            //
            // It also puts the caret back where the modal wants it: the
            // header's padding and the gap beside a short query are easy
            // misses, and a modal whose keystrokes go nowhere is
            // indistinguishable from one that has stopped responding. Mouse
            // *down*, so a click on a button inside still runs on the mouse up
            // that follows.
            .on_mouse_down(MouseButton::Left, {
                let target = target.clone();
                move |_event, window, cx| {
                    cx.stop_propagation();
                    window.focus(&target, cx);
                }
            })
            .children(self.child);

        if let Some(dismiss) = self.on_dismiss.clone() {
            if dismissible {
                panel = panel.on_action(move |_: &DismissModal, window, cx| dismiss(window, cx));
            }
        }
        if let Some(submit) = self.on_submit.clone() {
            panel = panel.on_action(move |_: &SubmitModal, window, cx| submit(window, cx));
        }
        if let Placement::Top(offset) = self.placement {
            panel = panel.mt(offset);
        }

        let mut scrim = div()
            // The id is not decoration: it is what registers a hitbox, and
            // without one every click here falls through to whatever the modal
            // is covering.
            .id(SharedString::from(format!("modal-scrim:{}", self.id)))
            .absolute()
            .inset_0()
            .flex()
            .flex_row()
            .justify_center()
            .map(|el| match self.placement {
                Placement::Center => el.items_center(),
                Placement::Top(_) => el.items_start(),
            })
            .child(panel);

        if matches!(self.scrim, Scrim::Dim) {
            scrim = scrim.bg(theme.overlay());
        }
        if let Some(dismiss) = self.on_dismiss {
            if dismissible {
                scrim = scrim.on_mouse_down(MouseButton::Left, move |_event, window, cx| {
                    dismiss(window, cx)
                });
            }
        }
        scrim
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIRST: &str = "first";
    const SECOND: &str = "second";

    /// The stacking rule, both halves: the modal that is already up answers a
    /// second request for *itself* by staying up, and a request for a
    /// different one is refused rather than stacked.
    #[test]
    fn one_modal_at_a_time_and_re_requesting_it_is_a_no_op() {
        assert_eq!(resolve(None, FIRST), Ok(Transition::Raise));
        assert_eq!(resolve(Some(FIRST), FIRST), Ok(Transition::AlreadyOpen));
        assert_eq!(
            resolve(Some(FIRST), SECOND),
            Err(ModalConflict {
                open: FIRST,
                requested: SECOND,
            })
        );
    }

    /// A conflict has to name both surfaces: "a dialog is already open" sends
    /// the reader looking for the wrong one.
    #[test]
    fn a_conflict_names_what_is_open_and_what_was_refused() {
        let conflict = ModalConflict {
            open: "Stop confirmation",
            requested: "command palette",
        };
        let message = conflict.to_string();
        assert!(message.contains("Stop confirmation"), "{message}");
        assert!(message.contains("command palette"), "{message}");
    }

    /// One predicate, read by escape and by the scrim alike — so a modal that
    /// escape cannot close cannot be clicked away either.
    #[test]
    fn escape_and_the_scrim_ask_the_same_question() {
        assert!(Dismissal::Dismissible.dismissible());
        assert!(!Dismissal::MustAnswer.dismissible());
    }

    /// Escape is bound one context deeper as well, which is the only depth at
    /// which it ties with gpuikit's own escape — and a tie is what
    /// registration order then breaks in our favour. Two spellings of one
    /// fact, pinned here.
    #[test]
    fn escape_is_also_bound_inside_a_field() {
        assert_eq!(MODAL_INPUT, format!("{MODAL_CONTEXT} > Input"));
    }
}
