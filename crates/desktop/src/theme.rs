//! Theme and styling system for Tasks Desktop.
//!
//! This module provides a consistent theming system matching the web frontend's
//! Tailwind CSS colors and styles. It includes:
//! - Color palette (semantic colors and state colors)
//! - Typography scale (font sizes, weights, line heights)
//! - Spacing scale
//! - Style helpers for common patterns

use gpui::{hsla, px, Hsla, Pixels, Rgba};
use models::task::TaskState;

// =============================================================================
// Color Palette
// =============================================================================

/// Semantic colors matching the web frontend's Tailwind CSS theme.
/// All colors are defined for dark mode (the default GPUI theme).
pub mod colors {

    // -------------------------------------------------------------------------
    // Base Colors (from web/src/index.css oklch values, converted to hex)
    // -------------------------------------------------------------------------

    /// Background color - very dark, almost black
    /// oklch(0.145 0 0) ≈ #1a1a1a
    pub const BACKGROUND: u32 = 0x1a1a1a;

    /// Foreground (text) color - almost white
    /// oklch(0.985 0 0) ≈ #fafafa
    pub const FOREGROUND: u32 = 0xfafafa;

    /// Card background color - same as background
    pub const CARD: u32 = 0x1a1a1a;

    /// Card foreground color
    pub const CARD_FOREGROUND: u32 = 0xfafafa;

    /// Primary color - white for dark mode
    pub const PRIMARY: u32 = 0xfafafa;

    /// Primary foreground - dark for contrast
    /// oklch(0.205 0 0) ≈ #2b2b2b
    pub const PRIMARY_FOREGROUND: u32 = 0x2b2b2b;

    /// Secondary color - dark gray
    /// oklch(0.269 0 0) ≈ #3f3f3f
    pub const SECONDARY: u32 = 0x3f3f3f;

    /// Secondary foreground
    pub const SECONDARY_FOREGROUND: u32 = 0xfafafa;

    /// Muted color - same as secondary
    pub const MUTED: u32 = 0x3f3f3f;

    /// Muted foreground - medium gray for secondary text
    /// oklch(0.708 0 0) ≈ #a3a3a3
    pub const MUTED_FOREGROUND: u32 = 0xa3a3a3;

    /// Accent color - same as secondary
    pub const ACCENT: u32 = 0x3f3f3f;

    /// Accent foreground
    pub const ACCENT_FOREGROUND: u32 = 0xfafafa;

    /// Border color - dark gray
    /// oklch(0.269 0 0) ≈ #3f3f3f
    pub const BORDER: u32 = 0x3f3f3f;

    /// Input border color
    pub const INPUT: u32 = 0x3f3f3f;

    /// Focus ring color
    /// oklch(0.556 0 0) ≈ #7c7c7c
    pub const RING: u32 = 0x7c7c7c;

    /// Destructive color - red
    /// oklch(0.396 0.141 25.723)
    pub const DESTRUCTIVE: u32 = 0x7f1d1d;

    /// Destructive foreground - lighter red for text
    /// oklch(0.637 0.237 25.331)
    pub const DESTRUCTIVE_FOREGROUND: u32 = 0xef4444;

    // -------------------------------------------------------------------------
    // State Colors (Tailwind colors for task states)
    // -------------------------------------------------------------------------

    /// Running state - Tailwind blue-600
    pub const STATE_RUNNING: u32 = 0x2563eb;

    /// Completed state - Tailwind green-600
    pub const STATE_COMPLETED: u32 = 0x16a34a;

    /// Question/waiting state - Tailwind yellow-600
    pub const STATE_QUESTION: u32 = 0xca8a04;

    /// Failed state - Tailwind red-600
    pub const STATE_FAILED: u32 = 0xdc2626;

    /// Pending state - Tailwind gray-500
    pub const STATE_PENDING: u32 = 0x6b7280;

    /// Merged/awaiting-merge state - Tailwind purple-600
    pub const STATE_MERGED: u32 = 0x9333ea;

    /// Cancelled state - Tailwind gray-400
    pub const STATE_CANCELLED: u32 = 0x9ca3af;
}

/// Convert a hex color to GPUI's Rgba format.
#[inline]
pub fn rgb(hex: u32) -> Rgba {
    gpui::rgb(hex)
}

/// Create a color with custom alpha from a hex value.
pub fn rgba(hex: u32, alpha: f32) -> Rgba {
    let r = ((hex >> 16) & 0xff) as f32 / 255.0;
    let g = ((hex >> 8) & 0xff) as f32 / 255.0;
    let b = (hex & 0xff) as f32 / 255.0;
    Rgba { r, g, b, a: alpha }
}

// =============================================================================
// Typography
// =============================================================================

/// Typography scale matching common web patterns.
pub mod typography {
    use super::*;

    // -------------------------------------------------------------------------
    // Font Sizes (in pixels, matching Tailwind's default scale)
    // -------------------------------------------------------------------------

    /// Extra small text (12px) - Tailwind text-xs
    pub const TEXT_XS: Pixels = px(12.0);

    /// Small text (14px) - Tailwind text-sm
    pub const TEXT_SM: Pixels = px(14.0);

    /// Base text (16px) - Tailwind text-base
    pub const TEXT_BASE: Pixels = px(16.0);

    /// Large text (18px) - Tailwind text-lg
    pub const TEXT_LG: Pixels = px(18.0);

    /// Extra large text (20px) - Tailwind text-xl
    pub const TEXT_XL: Pixels = px(20.0);

    /// 2XL text (24px) - Tailwind text-2xl
    pub const TEXT_2XL: Pixels = px(24.0);

    /// 3XL text (30px) - Tailwind text-3xl
    pub const TEXT_3XL: Pixels = px(30.0);

    /// 4XL text (36px) - Tailwind text-4xl
    pub const TEXT_4XL: Pixels = px(36.0);

    // -------------------------------------------------------------------------
    // Line Heights (as multipliers)
    // -------------------------------------------------------------------------

    /// Tight line height (1.25)
    pub const LINE_HEIGHT_TIGHT: f32 = 1.25;

    /// Snug line height (1.375)
    pub const LINE_HEIGHT_SNUG: f32 = 1.375;

    /// Normal line height (1.5)
    pub const LINE_HEIGHT_NORMAL: f32 = 1.5;

    /// Relaxed line height (1.625)
    pub const LINE_HEIGHT_RELAXED: f32 = 1.625;

    /// Loose line height (2.0)
    pub const LINE_HEIGHT_LOOSE: f32 = 2.0;

    // -------------------------------------------------------------------------
    // Font Weights (using GPUI's FontWeight)
    // -------------------------------------------------------------------------

    pub use gpui::FontWeight;

    /// Thin font weight (100)
    pub const WEIGHT_THIN: FontWeight = FontWeight::THIN;

    /// Extra light font weight (200)
    pub const WEIGHT_EXTRA_LIGHT: FontWeight = FontWeight::EXTRA_LIGHT;

    /// Light font weight (300)
    pub const WEIGHT_LIGHT: FontWeight = FontWeight::LIGHT;

    /// Normal font weight (400)
    pub const WEIGHT_NORMAL: FontWeight = FontWeight::NORMAL;

    /// Medium font weight (500)
    pub const WEIGHT_MEDIUM: FontWeight = FontWeight::MEDIUM;

    /// Semibold font weight (600)
    pub const WEIGHT_SEMIBOLD: FontWeight = FontWeight::SEMIBOLD;

    /// Bold font weight (700)
    pub const WEIGHT_BOLD: FontWeight = FontWeight::BOLD;

    /// Extra bold font weight (800)
    pub const WEIGHT_EXTRA_BOLD: FontWeight = FontWeight::EXTRA_BOLD;

    /// Black font weight (900)
    pub const WEIGHT_BLACK: FontWeight = FontWeight::BLACK;
}

// =============================================================================
// Spacing
// =============================================================================

/// Spacing scale matching Tailwind's default spacing values.
/// Each unit represents 4px (0.25rem at 16px base).
pub mod spacing {
    use super::*;

    /// 0px spacing
    pub const SPACE_0: Pixels = px(0.0);

    /// 1px spacing
    pub const SPACE_PX: Pixels = px(1.0);

    /// 2px spacing (0.5 unit)
    pub const SPACE_0_5: Pixels = px(2.0);

    /// 4px spacing (1 unit) - Tailwind p-1
    pub const SPACE_1: Pixels = px(4.0);

    /// 6px spacing (1.5 units)
    pub const SPACE_1_5: Pixels = px(6.0);

    /// 8px spacing (2 units) - Tailwind p-2
    pub const SPACE_2: Pixels = px(8.0);

    /// 10px spacing (2.5 units)
    pub const SPACE_2_5: Pixels = px(10.0);

    /// 12px spacing (3 units) - Tailwind p-3
    pub const SPACE_3: Pixels = px(12.0);

    /// 14px spacing (3.5 units)
    pub const SPACE_3_5: Pixels = px(14.0);

    /// 16px spacing (4 units) - Tailwind p-4
    pub const SPACE_4: Pixels = px(16.0);

    /// 20px spacing (5 units) - Tailwind p-5
    pub const SPACE_5: Pixels = px(20.0);

    /// 24px spacing (6 units) - Tailwind p-6
    pub const SPACE_6: Pixels = px(24.0);

    /// 28px spacing (7 units) - Tailwind p-7
    pub const SPACE_7: Pixels = px(28.0);

    /// 32px spacing (8 units) - Tailwind p-8
    pub const SPACE_8: Pixels = px(32.0);

    /// 36px spacing (9 units) - Tailwind p-9
    pub const SPACE_9: Pixels = px(36.0);

    /// 40px spacing (10 units) - Tailwind p-10
    pub const SPACE_10: Pixels = px(40.0);

    /// 44px spacing (11 units) - Tailwind p-11
    pub const SPACE_11: Pixels = px(44.0);

    /// 48px spacing (12 units) - Tailwind p-12
    pub const SPACE_12: Pixels = px(48.0);

    /// 56px spacing (14 units) - Tailwind p-14
    pub const SPACE_14: Pixels = px(56.0);

    /// 64px spacing (16 units) - Tailwind p-16
    pub const SPACE_16: Pixels = px(64.0);

    /// 80px spacing (20 units) - Tailwind p-20
    pub const SPACE_20: Pixels = px(80.0);

    /// 96px spacing (24 units) - Tailwind p-24
    pub const SPACE_24: Pixels = px(96.0);
}

// =============================================================================
// Border Radius
// =============================================================================

/// Border radius values matching Tailwind's scale.
pub mod radius {
    use super::*;

    /// No radius
    pub const NONE: Pixels = px(0.0);

    /// Small radius (4px) - Tailwind rounded-sm
    pub const SM: Pixels = px(4.0);

    /// Default radius (6px) - Tailwind rounded
    pub const DEFAULT: Pixels = px(6.0);

    /// Medium radius (8px) - Tailwind rounded-md
    pub const MD: Pixels = px(8.0);

    /// Large radius (12px) - Tailwind rounded-lg
    pub const LG: Pixels = px(12.0);

    /// Extra large radius (16px) - Tailwind rounded-xl
    pub const XL: Pixels = px(16.0);

    /// 2XL radius (24px) - Tailwind rounded-2xl
    pub const XXL: Pixels = px(24.0);

    /// Full radius (9999px) - Tailwind rounded-full
    pub const FULL: Pixels = px(9999.0);
}

// =============================================================================
// Style Helpers
// =============================================================================

/// Style helper traits and extensions for common patterns.
pub mod style_helpers {
    use gpui::{div, Div, Styled};

    use super::{colors, radius, rgb, spacing, typography, Pixels};

    /// Extension trait for applying common styles to elements.
    pub trait StyledExt: Styled + Sized {
        /// Apply background color from the theme.
        fn bg_theme(self) -> Self {
            self.bg(rgb(colors::BACKGROUND))
        }

        /// Apply card background color.
        fn bg_card(self) -> Self {
            self.bg(rgb(colors::CARD))
        }

        /// Apply accent background color.
        fn bg_accent(self) -> Self {
            self.bg(rgb(colors::ACCENT))
        }

        /// Apply muted background color.
        fn bg_muted(self) -> Self {
            self.bg(rgb(colors::MUTED))
        }

        /// Apply primary text color.
        fn text_primary(self) -> Self {
            self.text_color(rgb(colors::FOREGROUND))
        }

        /// Apply muted text color (for secondary text).
        fn text_muted(self) -> Self {
            self.text_color(rgb(colors::MUTED_FOREGROUND))
        }

        /// Apply destructive text color.
        fn text_destructive(self) -> Self {
            self.text_color(rgb(colors::DESTRUCTIVE_FOREGROUND))
        }

        /// Apply border with theme color.
        fn border_theme(self) -> Self {
            self.border_color(rgb(colors::BORDER))
        }

        // State colors

        /// Apply running state color (blue).
        fn text_running(self) -> Self {
            self.text_color(rgb(colors::STATE_RUNNING))
        }

        /// Apply completed state color (green).
        fn text_completed(self) -> Self {
            self.text_color(rgb(colors::STATE_COMPLETED))
        }

        /// Apply question state color (yellow).
        fn text_question(self) -> Self {
            self.text_color(rgb(colors::STATE_QUESTION))
        }

        /// Apply failed state color (red).
        fn text_failed(self) -> Self {
            self.text_color(rgb(colors::STATE_FAILED))
        }

        /// Apply pending state color (gray).
        fn text_pending(self) -> Self {
            self.text_color(rgb(colors::STATE_PENDING))
        }
    }

    /// Blanket impl for any GPUI element that implements `Styled`.
    impl<T: Styled + Sized> StyledExt for T {}

    /// Create a themed container div with standard padding and background.
    pub fn container() -> Div {
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(colors::BACKGROUND))
            .text_color(rgb(colors::FOREGROUND))
    }

    /// Create a card-style div with standard styling.
    pub fn card() -> Div {
        div()
            .flex()
            .flex_col()
            .bg(rgb(colors::CARD))
            .text_color(rgb(colors::CARD_FOREGROUND))
            .border_1()
            .border_color(rgb(colors::BORDER))
            .rounded(radius::LG)
            .p(spacing::SPACE_4)
    }

    /// Create a status indicator dot.
    pub fn status_dot(color: u32) -> Div {
        div()
            .w(spacing::SPACE_3)
            .h(spacing::SPACE_3)
            .rounded_full()
            .bg(rgb(color))
    }

    /// Create a badge with the given background color.
    pub fn badge(bg_color: u32) -> Div {
        div()
            .px(spacing::SPACE_2)
            .py(spacing::SPACE_1)
            .rounded(radius::SM)
            .bg(rgb(bg_color))
            .text_color(rgb(colors::FOREGROUND))
    }

    /// Create a heading with the specified size.
    pub fn heading(size: Pixels) -> Div {
        div()
            .text_size(size)
            .font_weight(typography::WEIGHT_SEMIBOLD)
            .text_color(rgb(colors::FOREGROUND))
    }

    /// Create muted/secondary text.
    pub fn muted_text() -> Div {
        div()
            .text_size(typography::TEXT_SM)
            .text_color(rgb(colors::MUTED_FOREGROUND))
    }
}

// =============================================================================
// Theme Struct (for future dynamic theming support)
// =============================================================================

/// A theme configuration that can be used for dynamic theming.
/// Currently provides the default dark theme, but structured to support
/// light mode or custom themes in the future.
#[derive(Clone)]
pub struct Theme {
    // Base colors
    pub background: u32,
    pub foreground: u32,
    pub card: u32,
    pub card_foreground: u32,
    pub primary: u32,
    pub primary_foreground: u32,
    pub secondary: u32,
    pub secondary_foreground: u32,
    pub muted: u32,
    pub muted_foreground: u32,
    pub accent: u32,
    pub accent_foreground: u32,
    pub border: u32,
    pub destructive: u32,
    pub destructive_foreground: u32,

    // State colors
    pub state_running: u32,
    pub state_completed: u32,
    pub state_question: u32,
    pub state_failed: u32,
    pub state_pending: u32,
    pub state_merged: u32,
    pub state_cancelled: u32,
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

impl Theme {
    /// Create the default dark theme.
    pub fn dark() -> Self {
        Self {
            background: colors::BACKGROUND,
            foreground: colors::FOREGROUND,
            card: colors::CARD,
            card_foreground: colors::CARD_FOREGROUND,
            primary: colors::PRIMARY,
            primary_foreground: colors::PRIMARY_FOREGROUND,
            secondary: colors::SECONDARY,
            secondary_foreground: colors::SECONDARY_FOREGROUND,
            muted: colors::MUTED,
            muted_foreground: colors::MUTED_FOREGROUND,
            accent: colors::ACCENT,
            accent_foreground: colors::ACCENT_FOREGROUND,
            border: colors::BORDER,
            destructive: colors::DESTRUCTIVE,
            destructive_foreground: colors::DESTRUCTIVE_FOREGROUND,
            state_running: colors::STATE_RUNNING,
            state_completed: colors::STATE_COMPLETED,
            state_question: colors::STATE_QUESTION,
            state_failed: colors::STATE_FAILED,
            state_pending: colors::STATE_PENDING,
            state_merged: colors::STATE_MERGED,
            state_cancelled: colors::STATE_CANCELLED,
        }
    }

    /// Get the appropriate state color for a task state.
    pub fn state_color(&self, state: &TaskState) -> u32 {
        match state {
            TaskState::Waiting | TaskState::Blocked => self.state_pending,
            TaskState::Running | TaskState::Testing => self.state_running,
            TaskState::Question => self.state_question,
            TaskState::AwaitingMerge => self.state_merged,
            TaskState::ChangesRequested => self.state_question, // Uses question color (amber)
            TaskState::Completed => self.state_completed,
            TaskState::Failed | TaskState::Conflict => self.state_failed,
            TaskState::Cancelled => self.state_cancelled,
        }
    }
}

// =============================================================================
// Component Theme (HSLA-based theme for UI primitive components)
// =============================================================================

/// HSLA-based theme for UI primitive components (Button, Badge, Card, Input).
///
/// Components that implement `IntoElement` (not `Render`) don't have access to
/// a GPUI context, so they accept this theme as a constructor parameter. This
/// allows theme propagation from parent views that *do* have access to the
/// global theme.
///
/// Colors use HSLA (matching shadcn/ui) so components can do lightness
/// arithmetic for hover states without converting color spaces.
#[derive(Clone, Debug)]
pub struct ComponentTheme {
    // Background colors
    pub background: Hsla,
    pub foreground: Hsla,
    pub card: Hsla,
    pub card_foreground: Hsla,

    // Primary colors
    pub primary: Hsla,
    pub primary_foreground: Hsla,

    // Secondary colors
    pub secondary: Hsla,
    pub secondary_foreground: Hsla,

    // Muted colors
    pub muted: Hsla,
    pub muted_foreground: Hsla,

    // Accent colors
    pub accent: Hsla,
    pub accent_foreground: Hsla,

    // Destructive colors
    pub destructive: Hsla,
    pub destructive_foreground: Hsla,

    // Border and input
    pub border: Hsla,
    pub input: Hsla,
    pub ring: Hsla,

    // Border radii
    pub radius_sm: Pixels,
    pub radius_md: Pixels,
    pub radius_lg: Pixels,
    pub radius_xl: Pixels,
}

impl Default for ComponentTheme {
    fn default() -> Self {
        Self::dark()
    }
}

impl ComponentTheme {
    /// Dark theme matching shadcn/ui defaults and the web frontend.
    pub fn dark() -> Self {
        Self {
            background: hsla(222.2 / 360.0, 0.84, 0.049, 1.0),
            foreground: hsla(210.0 / 360.0, 0.40, 0.98, 1.0),
            card: hsla(222.2 / 360.0, 0.84, 0.049, 1.0),
            card_foreground: hsla(210.0 / 360.0, 0.40, 0.98, 1.0),
            primary: hsla(210.0 / 360.0, 0.40, 0.98, 1.0),
            primary_foreground: hsla(222.2 / 360.0, 0.473, 0.112, 1.0),
            secondary: hsla(217.2 / 360.0, 0.327, 0.176, 1.0),
            secondary_foreground: hsla(210.0 / 360.0, 0.40, 0.98, 1.0),
            muted: hsla(217.2 / 360.0, 0.327, 0.176, 1.0),
            muted_foreground: hsla(215.0 / 360.0, 0.204, 0.651, 1.0),
            accent: hsla(217.2 / 360.0, 0.327, 0.176, 1.0),
            accent_foreground: hsla(210.0 / 360.0, 0.40, 0.98, 1.0),
            destructive: hsla(0.0, 0.625, 0.306, 1.0),
            destructive_foreground: hsla(210.0 / 360.0, 0.40, 0.98, 1.0),
            border: hsla(217.2 / 360.0, 0.327, 0.176, 1.0),
            input: hsla(217.2 / 360.0, 0.327, 0.176, 1.0),
            ring: hsla(212.7 / 360.0, 0.267, 0.839, 1.0),
            radius_sm: px(6.0),
            radius_md: px(8.0),
            radius_lg: px(10.0),
            radius_xl: px(12.0),
        }
    }

    /// Light theme matching shadcn/ui defaults.
    pub fn light() -> Self {
        Self {
            background: hsla(0.0, 0.0, 1.0, 1.0),
            foreground: hsla(222.2 / 360.0, 0.84, 0.049, 1.0),
            card: hsla(0.0, 0.0, 1.0, 1.0),
            card_foreground: hsla(222.2 / 360.0, 0.84, 0.049, 1.0),
            primary: hsla(222.2 / 360.0, 0.473, 0.112, 1.0),
            primary_foreground: hsla(210.0 / 360.0, 0.40, 0.98, 1.0),
            secondary: hsla(210.0 / 360.0, 0.40, 0.961, 1.0),
            secondary_foreground: hsla(222.2 / 360.0, 0.473, 0.112, 1.0),
            muted: hsla(210.0 / 360.0, 0.40, 0.961, 1.0),
            muted_foreground: hsla(215.4 / 360.0, 0.163, 0.469, 1.0),
            accent: hsla(210.0 / 360.0, 0.40, 0.961, 1.0),
            accent_foreground: hsla(222.2 / 360.0, 0.473, 0.112, 1.0),
            destructive: hsla(0.0, 0.843, 0.60, 1.0),
            destructive_foreground: hsla(210.0 / 360.0, 0.40, 0.98, 1.0),
            border: hsla(214.3 / 360.0, 0.318, 0.912, 1.0),
            input: hsla(214.3 / 360.0, 0.318, 0.912, 1.0),
            ring: hsla(222.2 / 360.0, 0.84, 0.049, 1.0),
            radius_sm: px(6.0),
            radius_md: px(8.0),
            radius_lg: px(10.0),
            radius_xl: px(12.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rgba_conversion() {
        let color = rgba(0xff0000, 0.5);
        assert!((color.r - 1.0).abs() < 0.001);
        assert!((color.g - 0.0).abs() < 0.001);
        assert!((color.b - 0.0).abs() < 0.001);
        assert!((color.a - 0.5).abs() < 0.001);
    }

    #[test]
    fn test_theme_state_colors() {
        let theme = Theme::dark();
        assert_eq!(
            theme.state_color(&TaskState::Running),
            colors::STATE_RUNNING
        );
        assert_eq!(
            theme.state_color(&TaskState::Testing),
            colors::STATE_RUNNING
        );
        assert_eq!(
            theme.state_color(&TaskState::Completed),
            colors::STATE_COMPLETED
        );
        assert_eq!(
            theme.state_color(&TaskState::Question),
            colors::STATE_QUESTION
        );
        assert_eq!(theme.state_color(&TaskState::Failed), colors::STATE_FAILED);
        assert_eq!(
            theme.state_color(&TaskState::Conflict),
            colors::STATE_FAILED
        );
        assert_eq!(
            theme.state_color(&TaskState::Waiting),
            colors::STATE_PENDING
        );
        assert_eq!(
            theme.state_color(&TaskState::Blocked),
            colors::STATE_PENDING
        );
        assert_eq!(
            theme.state_color(&TaskState::AwaitingMerge),
            colors::STATE_MERGED
        );
        assert_eq!(
            theme.state_color(&TaskState::Cancelled),
            colors::STATE_CANCELLED
        );
    }
}
