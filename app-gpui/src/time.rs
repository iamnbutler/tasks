//! Compact time formatting for list rows and live clocks.

use chrono::{DateTime, Utc};

/// "now", "5m", "3h", "2d" — the row-trailing relative timestamp.
pub fn relative(when: DateTime<Utc>) -> String {
    let seconds = (Utc::now() - when).num_seconds().max(0);
    match seconds {
        0..60 => "now".to_string(),
        60..3600 => format!("{}m", seconds / 60),
        3600..86_400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// "M:SS" elapsed since `since` — the live wall clock for running work
/// (a clock reads as working; a spinner reads as hung).
pub fn elapsed(since: DateTime<Utc>) -> String {
    let seconds = (Utc::now() - since).num_seconds().max(0);
    format!("{}:{:02}", seconds / 60, seconds % 60)
}
