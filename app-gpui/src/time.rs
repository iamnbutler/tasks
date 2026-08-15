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

/// `45s`, `12m30s`, `1h2m` — the shape `tasks status` prints, so an uptime
/// read in the Server window and one read in a terminal are the same string.
pub fn duration(d: chrono::Duration) -> String {
    let seconds = d.num_seconds().max(0);
    match seconds {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m{:02}s", s / 60, s % 60),
        s => format!("{}h{}m", s / 3600, (s % 3600) / 60),
    }
}

/// [`duration`] since a timestamp.
pub fn since(when: DateTime<Utc>) -> String {
    duration(Utc::now() - when)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_the_way_tasks_status_prints_them() {
        assert_eq!(duration(chrono::Duration::seconds(45)), "45s");
        assert_eq!(duration(chrono::Duration::seconds(750)), "12m30s");
        assert_eq!(duration(chrono::Duration::seconds(3720)), "1h2m");
        // Clock skew must not render as a negative age.
        assert_eq!(duration(chrono::Duration::seconds(-5)), "0s");
    }

    #[test]
    fn since_counts_from_a_timestamp() {
        assert_eq!(since(Utc::now()), "0s");
    }
}
