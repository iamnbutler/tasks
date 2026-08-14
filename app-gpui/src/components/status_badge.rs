//! Capsule status badges and the status→color vocabulary, matching the
//! Swift app: colored text on the same color at 15% opacity.

use gpui::prelude::*;
use gpui::{div, hsla, px, Hsla};
use tasks_client::api::models::TaskState;

fn color(hue_degrees: f32, saturation: f32, lightness: f32) -> Hsla {
    hsla(hue_degrees / 360., saturation, lightness, 1.)
}

pub fn task_state_color(state: TaskState) -> Hsla {
    match state {
        TaskState::Backlog => color(0., 0., 0.55),
        TaskState::Queued => color(30., 0.90, 0.60),
        TaskState::Scouting => color(210., 0.90, 0.62),
        TaskState::InReview => color(280., 0.70, 0.68),
        TaskState::ReadyToBuild => color(175., 0.60, 0.50),
        TaskState::Building => color(240., 0.65, 0.68),
        TaskState::Done => color(135., 0.55, 0.52),
        TaskState::Rejected => color(0., 0.80, 0.62),
    }
}

/// Human labels: `in_review` → "In Review".
pub fn title_case(wire: &str) -> String {
    wire.split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A capsule badge: `label` in `color` on a 15%-opacity wash of it.
pub fn status_badge(label: impl Into<String>, color: Hsla) -> impl IntoElement {
    div()
        .px(px(7.))
        .py(px(1.))
        .rounded_full()
        .bg(color.opacity(0.15))
        .text_color(color)
        .text_xs()
        .flex_none()
        .child(label.into())
}
