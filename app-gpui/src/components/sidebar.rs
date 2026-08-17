//! Dockable side panels.
//!
//! Modeled on Zed's workspace docks: the workspace owns each sidebar's
//! open/width state and registers the toggle actions; this component is
//! pure presentation plus a resize handle. The handle reports drag-start
//! through a callback — width updates during the drag are handled by the
//! workspace, which watches window-level mouse moves (a drag outruns the
//! handle's own hitbox immediately).

use gpui::prelude::*;
use gpui::{div, px, AnyElement, App, MouseDownEvent, Pixels, Window};
use gpuikit::theme::{ActiveTheme, Themeable};
use smallvec::SmallVec;

pub const DEFAULT_SIDEBAR_WIDTH: Pixels = px(240.);
pub const MIN_SIDEBAR_WIDTH: Pixels = px(160.);

/// A sidebar may take up to this share of the window — reading surfaces
/// like the inspector's spec view need real width, so the ceiling is
/// proportional, not a fixed pixel count.
pub const MAX_SIDEBAR_FRACTION: f32 = 0.5;

const RESIZE_HANDLE_WIDTH: Pixels = px(5.);

#[derive(PartialEq, Clone, Copy, Debug)]
pub enum SidebarSide {
    Left,
    Right,
}

/// Per-sidebar state owned by the workspace (Zed's `Dock` holds the same).
///
/// `open` is private on purpose. It has two kinds of writer — the user
/// talking about the panel (the toggle icon, ⌘B/⌘R) and content asking to be
/// seen (selecting a row) — and when they shared one field the second silently
/// undid the first: dismissing the inspector lasted until the next row click.
/// The verbs below keep that distinction, and privacy is what stops a future
/// `sidebar.open = true` from re-introducing the bug.
pub struct SidebarState {
    open: bool,
    /// The user closed this panel deliberately. Content may no longer force
    /// itself back into view until they say otherwise.
    dismissed: bool,
    pub width: Pixels,
}

impl SidebarState {
    pub fn new(open: bool) -> Self {
        Self {
            open,
            // Never `!open`: a panel that starts closed starts that way as a
            // default, not as something the user decided. Deriving it here
            // would ship an inspector that never opens at all.
            dismissed: false,
            width: DEFAULT_SIDEBAR_WIDTH,
        }
    }

    pub fn with_width(mut self, width: Pixels) -> Self {
        self.width = width;
        self
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The user acted on the panel itself — the title-bar icon, ⌘B/⌘R. This
    /// is the only path that records a dismissal.
    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.dismissed = !self.open;
    }

    /// Open regardless of a dismissal, and forget it. For a verb whose whole
    /// point is a surface only this panel renders — the user asked for
    /// something they cannot get anywhere else.
    pub fn force_open(&mut self) {
        self.open = true;
        self.dismissed = false;
    }

    /// Content asking to be seen. Honoured unless the user has dismissed the
    /// panel, in which case it stays out of the way and the panel's contents
    /// change underneath it.
    ///
    /// Unused since the v3 frame swap retired the inspector (its callers) —
    /// kept because it is half of the dismissal model this type exists for,
    /// and the chat pane will want it the first time content asks to be seen.
    #[allow(dead_code)]
    pub fn reveal(&mut self) {
        if !self.dismissed {
            self.open = true;
        }
    }

    /// Close the panel because its content went away. Not a statement about
    /// the panel, so it records nothing: the next `reveal` opens it again.
    ///
    /// Unused since the v3 frame swap, kept with [`Self::reveal`] as a pair.
    #[allow(dead_code)]
    pub fn hide(&mut self) {
        self.open = false;
    }

    /// Clamp to the legal range for the current window: at least
    /// [`MIN_SIDEBAR_WIDTH`], at most [`MAX_SIDEBAR_FRACTION`] of it.
    pub fn set_width(&mut self, width: Pixels, viewport_width: Pixels) {
        let max = viewport_width * MAX_SIDEBAR_FRACTION;
        self.width = width.clamp(MIN_SIDEBAR_WIDTH, max.max(MIN_SIDEBAR_WIDTH));
    }
}

type ResizeStartHandler = Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App) + 'static>;

#[derive(IntoElement)]
pub struct Sidebar {
    side: SidebarSide,
    width: Pixels,
    children: SmallVec<[AnyElement; 4]>,
    on_resize_start: Option<ResizeStartHandler>,
}

pub fn sidebar(side: SidebarSide, width: Pixels) -> Sidebar {
    Sidebar {
        side,
        width,
        children: SmallVec::new(),
        on_resize_start: None,
    }
}

impl Sidebar {
    pub fn child(mut self, child: impl IntoElement) -> Self {
        self.children.push(child.into_any_element());
        self
    }

    /// Called when the user presses down on the resize handle. The workspace
    /// takes over tracking from there.
    pub fn on_resize_start(
        mut self,
        handler: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_resize_start = Some(Box::new(handler));
        self
    }
}

impl RenderOnce for Sidebar {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme().clone();
        let side = self.side;

        let mut handle = div()
            .id(match side {
                SidebarSide::Left => "sidebar-resize-handle-left",
                SidebarSide::Right => "sidebar-resize-handle-right",
            })
            .absolute()
            .top_0()
            .bottom_0()
            .w(RESIZE_HANDLE_WIDTH)
            .cursor_col_resize()
            .hover({
                let handle_hover = theme.border_secondary();
                move |el| el.bg(handle_hover)
            });
        handle = match side {
            // The handle straddles the sidebar's inner edge.
            SidebarSide::Left => handle.right(-RESIZE_HANDLE_WIDTH / 2.),
            SidebarSide::Right => handle.left(-RESIZE_HANDLE_WIDTH / 2.),
        };
        if let Some(on_resize_start) = self.on_resize_start {
            handle = handle.on_mouse_down(gpui::MouseButton::Left, move |event, window, cx| {
                on_resize_start(event, window, cx);
            });
        }

        div()
            .relative()
            .flex()
            .flex_col()
            .w(self.width)
            .flex_none()
            .h_full()
            .overflow_hidden()
            .bg(theme.surface())
            .map(|el| match side {
                SidebarSide::Left => el.border_r_1(),
                SidebarSide::Right => el.border_l_1(),
            })
            .border_color(theme.border_subtle())
            .children(self.children)
            .child(handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The right sidebar's shape: closed at construction, but not *dismissed*
    /// — the first row click still has to open it.
    #[test]
    fn a_panel_that_starts_closed_still_opens_for_content() {
        let mut state = SidebarState::new(false);
        assert!(!state.is_open());
        state.reveal();
        assert!(state.is_open());
    }

    /// #899, literally: open by selecting a row, dismiss with the toggle,
    /// select another row. The panel stays out.
    #[test]
    fn a_dismissed_panel_ignores_the_next_selection() {
        let mut state = SidebarState::new(false);
        state.reveal();
        state.toggle();
        assert!(!state.is_open());
        state.reveal();
        assert!(
            !state.is_open(),
            "selecting a row re-opened a dismissed panel"
        );
    }

    /// The dismissal is durable, not a one-click grace period.
    #[test]
    fn a_dismissal_outlasts_repeated_selections() {
        let mut state = SidebarState::new(false);
        state.reveal();
        state.toggle();
        for _ in 0..5 {
            state.reveal();
        }
        assert!(!state.is_open());
    }

    /// Toggling back open is the user changing their mind — the dismissal is
    /// forgotten, so content can reveal the panel again afterwards.
    #[test]
    fn toggling_back_open_forgets_the_dismissal() {
        let mut state = SidebarState::new(false);
        state.reveal();
        state.toggle();
        state.toggle();
        assert!(state.is_open());

        state.hide();
        state.reveal();
        assert!(state.is_open());
    }

    /// `begin_review` focuses a field that only this panel renders, so it
    /// overrides a dismissal — and clears it, rather than leaving the panel
    /// open with a dismissal still pending.
    #[test]
    fn force_open_overrides_and_clears_a_dismissal() {
        let mut state = SidebarState::new(false);
        state.toggle();
        state.toggle();
        assert!(!state.is_open());

        state.force_open();
        assert!(state.is_open());

        state.hide();
        state.reveal();
        assert!(state.is_open(), "force_open left the dismissal in place");
    }

    /// Escape and the inspector's own ✕ clear the selection and the panel
    /// follows its content out. That is not a statement about the panel, so
    /// clicking a row afterwards opens it again.
    #[test]
    fn hiding_is_not_a_dismissal() {
        let mut state = SidebarState::new(false);
        state.reveal();
        state.hide();
        assert!(!state.is_open());
        state.reveal();
        assert!(state.is_open());
    }

    /// The left sidebar's shape: open at construction, so its first toggle is
    /// the dismissal.
    #[test]
    fn a_panel_that_starts_open_dismisses_on_its_first_toggle() {
        let mut state = SidebarState::new(true);
        assert!(state.is_open());
        state.toggle();
        assert!(!state.is_open());
        state.reveal();
        assert!(!state.is_open());
    }

    #[test]
    fn width_is_clamped_to_the_windows_legal_range() {
        let mut state = SidebarState::new(true);
        let viewport = px(1000.);

        state.set_width(px(20.), viewport);
        assert_eq!(state.width, MIN_SIDEBAR_WIDTH);

        state.set_width(px(900.), viewport);
        assert_eq!(state.width, viewport * MAX_SIDEBAR_FRACTION);

        // A window narrower than the minimum: the floor wins, so the panel
        // never collapses to nothing.
        state.set_width(px(20.), px(100.));
        assert_eq!(state.width, MIN_SIDEBAR_WIDTH);
    }
}
