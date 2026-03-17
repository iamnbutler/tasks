//! Dashboard view — the main landing page of the app.
//!
//! Displays:
//! - Stats cards (active sessions, running tasks, waiting tasks, merge queue)
//! - Active tasks list (running, question, testing, awaiting_merge)
//! - Recent events list

use chrono::{DateTime, Utc};
use gpui::{div, prelude::*, Entity, FontWeight, Styled, Window};

use crate::api::{self, TaskState};
use crate::components::{Badge, BadgeVariant, Card, CardContent, CardHeader};
use crate::state::AppState;
use crate::theme::{colors, radius, rgb, spacing, style_helpers::StyledExt, typography, ComponentTheme};

/// Maximum number of recent events to display.
const MAX_RECENT_EVENTS: usize = 20;

/// The Dashboard view component.
pub struct Dashboard {
    state: Entity<AppState>,
    theme: ComponentTheme,
}

impl Dashboard {
    pub fn new(state: Entity<AppState>) -> Self {
        Self {
            state,
            theme: ComponentTheme::default(),
        }
    }
}

impl Render for Dashboard {
    fn render(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let state = self.state.read(cx);

        // Check if we have data
        let Some(snapshot) = state.snapshot() else {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_muted()
                        .text_size(typography::TEXT_SM)
                        .child("No data yet"),
                )
                .into_any_element();
        };

        // Calculate stats
        let active_sessions = snapshot.slot_utilization.active;
        let max_sessions = snapshot.slot_utilization.max;
        let running_count = state.count_by_state(TaskState::Running);
        let waiting_count = state.count_by_state(TaskState::Waiting);
        let merge_queue_count = state.filtered_merge_queue().len();

        // Get active tasks and recent events
        let active_tasks = state.active_tasks();
        let recent_events = state.recent_events(MAX_RECENT_EVENTS);

        // Check if viewing all projects (no project filter selected)
        let show_project = state.selected_project().is_none();

        let theme = self.theme.clone();

        // Collect task/event data into owned values before building the element tree,
        // since the borrowed `state` cannot live across the `.child()` chain.
        let task_data: Vec<_> = active_tasks
            .iter()
            .map(|t| TaskRowData {
                state: t.state,
                title: t.title.clone(),
                project: t.project.clone(),
                updated_at: t.updated_at,
            })
            .collect();

        let event_data: Vec<_> = recent_events
            .iter()
            .map(|e| EventRowData {
                ts: e.ts,
                event_type: e.event_type.as_str().to_string(),
                actor: api::actor_display_name(&e.actor).to_string(),
                task_id: truncate_id(&e.task),
                data_preview: event_data_preview(&e.data),
            })
            .collect();

        div()
            .size_full()
            .p(spacing::SPACE_6)
            .flex()
            .flex_col()
            .gap(spacing::SPACE_6)
            .bg_theme()
            .child(
                // Stats cards row
                div()
                    .flex()
                    .gap(spacing::SPACE_4)
                    .child(stat_card(
                        "Active Sessions",
                        &format!("{}", active_sessions),
                        Some(&format!("/ {}", max_sessions)),
                        &theme,
                    ))
                    .child(stat_card(
                        "Running Tasks",
                        &running_count.to_string(),
                        None,
                        &theme,
                    ))
                    .child(stat_card(
                        "Waiting Tasks",
                        &waiting_count.to_string(),
                        None,
                        &theme,
                    ))
                    .child(stat_card(
                        "Merge Queue",
                        &merge_queue_count.to_string(),
                        None,
                        &theme,
                    )),
            )
            .child(
                // Active Tasks section
                div()
                    .flex()
                    .flex_col()
                    .gap(spacing::SPACE_2)
                    .child(section_heading("Active Tasks"))
                    .child(if task_data.is_empty() {
                        div()
                            .text_muted()
                            .text_size(typography::TEXT_SM)
                            .child("No active tasks right now.")
                            .into_any_element()
                    } else {
                        div()
                            .flex()
                            .flex_col()
                            .gap(spacing::SPACE_2)
                            .children(
                                task_data
                                    .into_iter()
                                    .map(|td| task_row(&td, show_project, &theme)),
                            )
                            .into_any_element()
                    }),
            )
            .child(
                // Recent Events section
                div()
                    .flex()
                    .flex_col()
                    .gap(spacing::SPACE_2)
                    .child(section_heading("Recent Events"))
                    .child(if event_data.is_empty() {
                        div()
                            .text_muted()
                            .text_size(typography::TEXT_SM)
                            .child("No events yet.")
                            .into_any_element()
                    } else {
                        div()
                            .flex()
                            .flex_col()
                            .gap(spacing::SPACE_1)
                            .children(event_data.into_iter().map(|ed| event_row(&ed)))
                            .into_any_element()
                    }),
            )
            .into_any_element()
    }
}

// =============================================================================
// Section heading helper
// =============================================================================

fn section_heading(text: &str) -> gpui::Div {
    div()
        .text_size(typography::TEXT_BASE)
        .font_weight(typography::WEIGHT_SEMIBOLD)
        .text_primary()
        .child(text.to_string())
}

// =============================================================================
// Stat card helper (using main's Card component)
// =============================================================================

fn stat_card(title: &str, value: &str, suffix: Option<&str>, theme: &ComponentTheme) -> gpui::Div {
    let mut value_el = div()
        .text_size(typography::TEXT_XL)
        .font_weight(typography::WEIGHT_BOLD)
        .text_primary()
        .child(value.to_string());

    if let Some(s) = suffix {
        value_el = value_el.child(
            div()
                .text_muted()
                .text_size(typography::TEXT_SM)
                .font_weight(typography::WEIGHT_NORMAL)
                .child(format!(" {}", s)),
        );
    }

    Card::new()
        .theme(theme.clone())
        .child(
            CardHeader::new().child(
                div()
                    .text_size(typography::TEXT_SM)
                    .text_muted()
                    .font_weight(typography::WEIGHT_MEDIUM)
                    .child(title.to_string()),
            ),
        )
        .child(CardContent::new().child(value_el))
        .into_element()
}

// =============================================================================
// Task row
// =============================================================================

/// Owned data for rendering a task row (avoids borrow conflicts).
struct TaskRowData {
    state: TaskState,
    title: String,
    project: String,
    updated_at: DateTime<Utc>,
}

fn task_state_badge(state: TaskState, theme: &ComponentTheme) -> Badge {
    let (label, variant) = match state {
        TaskState::Running => ("running", BadgeVariant::Default),
        TaskState::Question => ("question", BadgeVariant::Secondary),
        TaskState::Testing => ("testing", BadgeVariant::Outline),
        TaskState::AwaitingMerge => ("awaiting merge", BadgeVariant::Secondary),
        TaskState::Completed => ("completed", BadgeVariant::Default),
        TaskState::Failed => ("failed", BadgeVariant::Destructive),
        TaskState::Waiting => ("waiting", BadgeVariant::Outline),
        TaskState::Blocked => ("blocked", BadgeVariant::Outline),
        TaskState::Conflict => ("conflict", BadgeVariant::Destructive),
        TaskState::Cancelled => ("cancelled", BadgeVariant::Ghost),
    };

    Badge::new(label).variant(variant).theme(theme.clone())
}

fn task_row(data: &TaskRowData, show_project: bool, theme: &ComponentTheme) -> gpui::Div {
    let badge = task_state_badge(data.state, theme);
    let updated_at = format_relative_time(&data.updated_at);

    let mut row = div()
        .rounded(radius::LG)
        .border_1()
        .border_color(rgb(colors::BORDER))
        .bg(rgb(colors::CARD))
        .p(spacing::SPACE_2)
        .flex()
        .items_center()
        .gap(spacing::SPACE_2)
        .child(badge)
        .child(
            div()
                .flex_1()
                .text_primary()
                .font_weight(FontWeight::MEDIUM)
                .overflow_hidden()
                .text_ellipsis()
                .child(data.title.clone()),
        );

    if show_project {
        row = row.child(
            div()
                .text_muted()
                .text_size(typography::TEXT_SM)
                .flex_shrink_0()
                .child(data.project.clone()),
        );
    }

    row.child(
        div()
            .text_muted()
            .text_size(typography::TEXT_SM)
            .flex_shrink_0()
            .child(updated_at),
    )
}

// =============================================================================
// Event row
// =============================================================================

/// Owned data for rendering an event row.
struct EventRowData {
    ts: DateTime<Utc>,
    event_type: String,
    actor: String,
    task_id: String,
    data_preview: String,
}

fn event_row(data: &EventRowData) -> gpui::Div {
    let timestamp = format_relative_time(&data.ts);
    let theme = ComponentTheme::default();

    let mut row = div()
        .rounded(radius::LG)
        .border_1()
        .border_color(rgb(colors::BORDER))
        .bg(rgb(colors::CARD))
        .px(spacing::SPACE_2)
        .py(spacing::SPACE_1)
        .flex()
        .items_start()
        .gap(spacing::SPACE_2)
        .text_size(typography::TEXT_SM)
        .child(
            div()
                .text_muted()
                .flex_shrink_0()
                .pt(gpui::px(2.0))
                .child(timestamp),
        )
        .child(
            Badge::new(data.event_type.as_str())
                .outline()
                .theme(theme),
        )
        .child(
            div()
                .text_muted()
                .flex_shrink_0()
                .child(data.actor.clone()),
        );

    if !data.task_id.is_empty() {
        row = row.child(
            div()
                .text_muted()
                .flex_shrink_0()
                .child(data.task_id.clone()),
        );
    }

    row.child(
        div()
            .text_muted()
            .flex_1()
            .overflow_hidden()
            .text_ellipsis()
            .child(data.data_preview.clone()),
    )
}

// =============================================================================
// Helpers
// =============================================================================

/// Truncate a task ID to at most 8 characters for display.
fn truncate_id(id: &str) -> String {
    if id.len() > 8 {
        id[..8].to_string()
    } else {
        id.to_string()
    }
}

/// Format a `DateTime<Utc>` as a human-friendly relative time string.
fn format_relative_time(dt: &DateTime<Utc>) -> String {
    let now = Utc::now();
    let duration = now.signed_duration_since(*dt);

    if duration.num_seconds() < 60 {
        return "just now".to_string();
    }

    if duration.num_minutes() < 60 {
        let mins = duration.num_minutes();
        return format!("{}m ago", mins);
    }

    if duration.num_hours() < 24 {
        let hours = duration.num_hours();
        return format!("{}h ago", hours);
    }

    let days = duration.num_days();
    format!("{}d ago", days)
}

/// Generate a preview of event data, safely truncated to ~80 characters.
///
/// Uses `char_indices` to find a valid UTF-8 boundary rather than
/// slicing by byte offset (which would panic on multi-byte characters).
fn event_data_preview(data: &serde_json::Value) -> String {
    let raw = data.to_string();
    if raw.len() <= 80 {
        return raw;
    }
    let boundary = raw
        .char_indices()
        .nth(80)
        .map(|(i, _)| i)
        .unwrap_or(raw.len());
    format!("{}...", &raw[..boundary])
}
