//! The orchestrator's context window, as something a human can read at a
//! glance.
//!
//! Everything here is a pure function of [`OrchestratorSessionInfo`], which is
//! the whole point: what the gauge says — and, more importantly, what it
//! refuses to say when the server hasn't reported a window yet — is testable
//! without standing up a gpui `App`. The view in `workspace.rs` only paints
//! what [`Gauge::rows`] and [`Gauge::headline`] return.
//!
//! Two rules the shape follows:
//!
//! - **No window, no percentage.** The context window is transcribed from the
//!   agent, not derived from the model name, so before a tick has reported one
//!   there is no denominator. The reading is still worth showing on its own;
//!   an invented denominator would turn an honest number into a confident
//!   fiction, and 41% of the wrong window is less useful than no percentage at
//!   all.
//! - **The bill is never drawn in the bar.** `tick_tokens` routinely runs to
//!   several times the window because every internal turn re-reads the cached
//!   prefix, so it appears under its own heading, in its own words, below the
//!   segments — never as a share of anything.

use tasks_client::api::models::{ContextBreakdown, OrchestratorSessionInfo};

/// One band of the context bar, and one row of the breakdown under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Band {
    /// Served from the prompt cache — nearly all of a long resumed session.
    CacheRead,
    /// Written into the cache by the last call.
    CacheCreation,
    /// Fresh tokens, sent in full.
    Input,
    /// What the window has left.
    Free,
}

impl Band {
    pub fn label(self) -> &'static str {
        match self {
            Band::CacheRead => "Cached",
            Band::CacheCreation => "Newly cached",
            Band::Input => "Fresh input",
            Band::Free => "Free space",
        }
    }
}

/// One breakdown row: a band, its size, and its share of the window when
/// there is a window to take a share of.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Row {
    pub band: Band,
    pub tokens: i64,
    /// 0.0–1.0 of the *window*, not of the reading — so the segments and the
    /// bar agree, and the four rows total 100%.
    pub share: Option<f32>,
}

/// The gauge's view of one session.
#[derive(Debug, Clone, PartialEq)]
pub struct Gauge {
    /// Display name of the model the reading was taken on ("Opus 5"), or the
    /// raw wire id when it isn't one this knows how to shorten.
    pub model: Option<String>,
    /// What the session is holding, in tokens.
    pub tokens: i64,
    pub window: Option<i64>,
    breakdown: Option<ContextBreakdown>,
    /// What the last tick spent. A bill; see the module note.
    pub tick_tokens: Option<i64>,
    pub compactions: i64,
}

impl Gauge {
    /// `None` until a tick has taken a reading — before that there is nothing
    /// to show, and a `0 tokens` gauge would read as an empty conversation
    /// rather than an unmeasured one.
    pub fn new(info: &OrchestratorSessionInfo) -> Option<Self> {
        Some(Self {
            model: info.model_id.as_deref().map(model_name),
            tokens: info.context_tokens?,
            window: info.context_window,
            breakdown: info.context_breakdown,
            tick_tokens: info.tick_tokens,
            compactions: info.compactions,
        })
    }

    /// 0.0–1.0, or `None` with no window reported.
    pub fn fraction(&self) -> Option<f32> {
        let window = self.window?;
        (window > 0).then(|| (self.tokens as f32 / window as f32).clamp(0., 1.))
    }

    /// `410.1k / 1M · 41%`, degrading to `410.1k` with no window. The one
    /// string the collapsed pill shows.
    pub fn headline(&self) -> String {
        let tokens = tokens(self.tokens);
        match (self.window, self.percent()) {
            (Some(window), Some(percent)) => {
                format!("{tokens} / {} · {percent}", self::tokens(window))
            }
            _ => tokens,
        }
    }

    /// `41%`, and `<1%` for a reading that is real but rounds away — a fresh
    /// session against a 1M window is 0.4%, and `0%` reads as "not measured".
    pub fn percent(&self) -> Option<String> {
        let fraction = self.fraction()?;
        Some(match fraction * 100. {
            p if p > 0. && p < 1. => "<1%".to_string(),
            p => format!("{}%", p.round() as i64),
        })
    }

    /// The four bands, in bar order: cache first because on a resumed session
    /// it is nearly the whole thing, free space last.
    ///
    /// Empty when the server reported a total without its parts — which is
    /// what an older server does, and is why the bar is drawn from these rows
    /// rather than from the total: one code path, and a version skew loses the
    /// breakdown instead of drawing a bar of nothing.
    pub fn rows(&self) -> Vec<Row> {
        let Some(parts) = self.breakdown else {
            return Vec::new();
        };
        let share = |tokens: i64| {
            self.window
                .filter(|w| *w > 0)
                .map(|w| (tokens as f32 / w as f32).clamp(0., 1.))
        };
        let mut rows = vec![
            Row {
                band: Band::CacheRead,
                tokens: parts.cache_read,
                share: share(parts.cache_read),
            },
            Row {
                band: Band::CacheCreation,
                tokens: parts.cache_creation,
                share: share(parts.cache_creation),
            },
            Row {
                band: Band::Input,
                tokens: parts.input,
                share: share(parts.input),
            },
        ];
        // Free space is a fact about the window, so it exists only when one
        // was reported — and never goes negative, which would otherwise be the
        // reading a compaction is about to fix.
        if let Some(window) = self.window {
            rows.push(Row {
                band: Band::Free,
                tokens: (window - self.tokens).max(0),
                share: share((window - self.tokens).max(0)),
            });
        }
        rows
    }
}

/// `410.1k`, `1M`, `43.7k`, `812`.
///
/// One decimal above a thousand and none below it, and a whole number of
/// millions loses its `.0` — the gauge's denominator is almost always exactly
/// `1M`, and `1.0M` reads as an approximation of something it is not.
pub fn tokens(n: i64) -> String {
    const K: f64 = 1_000.;
    const M: f64 = 1_000_000.;
    let v = n as f64;
    if v.abs() < K {
        format!("{n}")
    } else if v.abs() < M {
        format!("{:.1}k", v / K)
    } else {
        let millions = v / M;
        if (millions - millions.round()).abs() < f64::EPSILON {
            format!("{}M", millions.round() as i64)
        } else {
            format!("{millions:.1}M")
        }
    }
}

/// `claude-opus-5[1m]` → `Opus 5`, `claude-haiku-4-5-20251001` → `Haiku 4.5`.
///
/// A shortener, not a lookup table: it has no list of models, so a model that
/// ships tomorrow reads correctly today. Anything it cannot parse confidently
/// comes back whole — a wire id in the UI is ugly, and a wrong name is worse.
///
/// The `[1m]` suffix is dropped because the window is displayed beside the
/// name as an actual number, which is the thing the suffix was standing in for.
pub fn model_name(id: &str) -> String {
    let base = id.split('[').next().unwrap_or(id);
    let Some(rest) = base.strip_prefix("claude-") else {
        return id.to_string();
    };
    let mut parts = rest.split('-');
    let Some(family) = parts.next().filter(|f| !f.is_empty()) else {
        return id.to_string();
    };
    let mut version = Vec::new();
    for part in parts {
        // A trailing datestamp is a release date, not a version.
        if part.len() == 8 && part.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if !part.chars().all(|c| c.is_ascii_digit()) || part.is_empty() {
            return id.to_string();
        }
        version.push(part);
    }
    let mut name = family[..1].to_uppercase() + &family[1..];
    if !version.is_empty() {
        name.push(' ');
        name.push_str(&version.join("."));
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn info(
        context: Option<i64>,
        window: Option<i64>,
        breakdown: Option<ContextBreakdown>,
    ) -> OrchestratorSessionInfo {
        OrchestratorSessionInfo {
            cc_session_id: Some("s".into()),
            workdir: None,
            checked_out: false,
            lane: Default::default(),
            context_tokens: context,
            tick_tokens: Some(1_634_166),
            model_id: Some("claude-opus-5[1m]".into()),
            context_window: window,
            context_breakdown: breakdown,
            compactions: 1,
            last_compacted_at: Some(Utc::now()),
        }
    }

    #[test]
    fn token_counts_read_the_way_the_agent_reports_them() {
        assert_eq!(tokens(0), "0");
        assert_eq!(tokens(812), "812");
        assert_eq!(tokens(999), "999");
        assert_eq!(tokens(1_000), "1.0k");
        assert_eq!(tokens(410_131), "410.1k");
        // A whole million is the denominator, and it is exact.
        assert_eq!(tokens(1_000_000), "1M");
        assert_eq!(tokens(200_000), "200.0k");
        assert_eq!(tokens(1_634_166), "1.6M");
    }

    #[test]
    fn model_names_shorten_without_a_table_and_never_guess() {
        assert_eq!(model_name("claude-opus-5[1m]"), "Opus 5");
        assert_eq!(model_name("claude-sonnet-5"), "Sonnet 5");
        assert_eq!(model_name("claude-haiku-4-5-20251001"), "Haiku 4.5");
        assert_eq!(model_name("claude-fable-5"), "Fable 5");
        // Nothing it can take apart confidently comes back whole rather than
        // mangled: a wire id is ugly, a wrong name is a lie.
        assert_eq!(model_name("gpt-4o"), "gpt-4o");
        assert_eq!(model_name("claude-opus-next"), "claude-opus-next");
        assert_eq!(model_name("claude-"), "claude-");
    }

    /// The load-bearing refusal: no window means no percentage, at every level
    /// of the view, and the reading itself still shows.
    #[test]
    fn a_reading_without_a_window_reports_tokens_and_no_percentage() {
        let parts = ContextBreakdown {
            input: 1_200,
            cache_read: 180_000,
            cache_creation: 800,
        };
        let gauge = Gauge::new(&info(Some(182_000), None, Some(parts))).expect("a reading");
        assert_eq!(gauge.headline(), "182.0k");
        assert_eq!(gauge.percent(), None);
        assert_eq!(gauge.fraction(), None);
        // The parts are still worth showing; only their share of nothing goes.
        let rows = gauge.rows();
        assert_eq!(rows.len(), 3, "no window, so no free-space row");
        assert!(rows.iter().all(|r| r.share.is_none()));
        assert!(!rows.iter().any(|r| r.band == Band::Free));
    }

    #[test]
    fn a_full_reading_splits_the_window_into_four_bands_that_total_it() {
        let parts = ContextBreakdown {
            input: 1_200,
            cache_read: 400_000,
            cache_creation: 8_800,
        };
        let gauge =
            Gauge::new(&info(Some(410_000), Some(1_000_000), Some(parts))).expect("a reading");
        assert_eq!(gauge.headline(), "410.0k / 1M · 41%");

        let rows = gauge.rows();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].band, Band::CacheRead, "cache leads the bar");
        assert_eq!(rows[3].band, Band::Free);
        assert_eq!(rows[3].tokens, 590_000);
        let total: i64 = rows.iter().map(|r| r.tokens).sum();
        assert_eq!(total, 1_000_000, "the bands are the whole window");
        let shares: f32 = rows.iter().filter_map(|r| r.share).sum();
        assert!((shares - 1.0).abs() < 0.001, "{shares}");
    }

    /// Two ways the gauge could lie by rounding, in opposite directions.
    #[test]
    fn a_small_reading_is_under_one_percent_and_an_overfull_one_does_not_go_negative() {
        let small = Gauge::new(&info(Some(3_000), Some(1_000_000), None)).expect("a reading");
        assert_eq!(
            small.percent().as_deref(),
            Some("<1%"),
            "0% would read as unmeasured"
        );

        // A reading taken before a compaction can exceed the window the next
        // turn reports; free space is zero, never a negative band.
        let parts = ContextBreakdown {
            input: 0,
            cache_read: 1_100_000,
            cache_creation: 0,
        };
        let over =
            Gauge::new(&info(Some(1_100_000), Some(1_000_000), Some(parts))).expect("a reading");
        assert_eq!(over.percent().as_deref(), Some("100%"));
        assert_eq!(over.rows()[3].tokens, 0);
    }

    #[test]
    fn an_unmeasured_session_has_no_gauge_at_all() {
        assert!(Gauge::new(&info(None, Some(1_000_000), None)).is_none());
    }
}
