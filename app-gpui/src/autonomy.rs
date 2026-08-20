//! What pressing Play actually does, said once, before the first press (#993).
//!
//! Play starts virtual machines on this Mac, spends Anthropic API credit,
//! pushes branches and opens pull requests — and, with the charter as it
//! ships, merges those pull requests and closes the issues behind them. None
//! of it asks. Nothing in the app said so before the click, and the charter
//! that governs the sharp half was not rendered anywhere, so the answer to
//! "how do I stop it merging things" was `curl`.
//!
//! **This is not a gate.** No server row is consulted before an action, no
//! endpoint refuses because the notice is unread, and `POST /autonomy-notice`
//! records that a person was shown something rather than that they agreed to
//! it. Three properties keep it from drifting into one: the record lives on
//! the server and nothing there reads it back; [`intercepts`] fires at most
//! once per install and never for any mode but [`Mode::Play`]; and it returns
//! "carry on" for every uncertainty there is — an unreachable server, a
//! platform that refuses the window, a question nobody has answered yet. A
//! press is never swallowed.
//!
//! **Generated from the charter rows, not written as prose.** The nine
//! capabilities are the thing the server actually enforces, so a notice that
//! restated them in its own words would be a second source of truth that goes
//! stale the first time somebody narrows the charter. What is *not* generated
//! is [`ALWAYS`]: mode gates dispatch, and a charter narrowed to nothing still
//! spends the machine, so a notice assembled only from capabilities would read
//! as "Play does nothing now".
//!
//! The static claims come from [`crate::disclaimer`] rather than being written
//! again here — see [`Notice`].

use chrono::{DateTime, Utc};
use gpui::prelude::*;
use gpui::{
    div, px, size, App, Bounds, Context, Entity, Global, Render, TitlebarOptions, Window,
    WindowBounds, WindowHandle, WindowOptions,
};
use gpuikit::theme::{ActiveTheme, Themeable};
use tasks_client::api::models::{Capability, CharterEntry, CharterLevel, Mode};

use crate::disclaimer;
use crate::state::AppState;

/// What Play does whatever the charter says.
///
/// Deliberately **not** generated from the capabilities. The charter governs
/// what the *orchestrator* may do; mode governs whether the pipeline
/// dispatches at all, and a scout or a build that runs spends these three
/// regardless of how narrow the charter is.
///
/// It also deliberately does not restate [`disclaimer::PIPELINE_CAUTION`],
/// which the notice quotes verbatim beside it: that sentence names the
/// server's GitHub acts, and these name what happens on the machine and on
/// the bill. One claim, one source, in both directions.
pub const ALWAYS: &[&str] = &[
    "start virtual machines on this Mac and run coding agents inside them, \
     with permission checks off",
    "spend Anthropic API credit — there is no cap, and nothing asks before a run",
    "keep going on its own until you pause it",
];

/// One way to stop it, named by the command that does it.
///
/// The `command` is a [`crate::commands::COMMANDS`] id, not a label: the
/// notice renders the registry's own name for it, so renaming a menu item
/// moves the text here rather than leaving the notice pointing at something
/// that is not there.
pub struct OffSwitch {
    pub command: &'static str,
    /// What choosing it does, in the notice's voice.
    pub effect: &'static str,
}

/// The off switches, in the order somebody looking for one would want them:
/// stop the bleeding, then stop it properly, then narrow what it may do at
/// all, then kill what is running right now.
pub const OFF_SWITCHES: &[OffSwitch] = &[
    OffSwitch {
        command: "mode-pause",
        effect: "no new work starts; anything already running finishes",
    },
    OffSwitch {
        command: "mode-stop",
        effect: "the same, and the pipeline stays down until you start it again",
    },
    OffSwitch {
        command: "charter",
        effect: "turn any of the nine off one at a time, without stopping the rest",
    },
    OffSwitch {
        command: "kill-all-containers",
        effect: "cancel every scout and build that is running now",
    },
];

/// The registry's current name for an off switch, prefixed with the menu it
/// lives under — "Server ▸ Pipeline: Pause".
pub fn off_switch_label(switch: &OffSwitch) -> String {
    match crate::commands::by_id(switch.command) {
        Some(command) => {
            let facts = crate::commands::Facts::for_menu_bar(crate::menus::MenuState::default());
            match command.menu {
                Some(slot) => format!("{} ▸ {}", slot.menu_name(), command.label(facts)),
                None => command.label(facts).to_string(),
            }
        }
        // Unreachable while the test below passes, and a blank line is a
        // worse answer than an honest one.
        None => format!("(missing command: {})", switch.command),
    }
}

/// The nine, sorted by what the charter currently grants.
///
/// **Three buckets, not two.** Collapsing `shadow` into "can" claims an effect
/// that never happens; collapsing it into "cannot" hides a judgment that is
/// still being made and still being recorded. Neither is what a shadowed
/// capability is.
///
/// Each list is in [`Capability::BY_CONSEQUENCE`] order, so the sharpest thing
/// in a bucket is the first thing read.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Permissions {
    /// `live` — applied, without asking.
    pub on_its_own: Vec<Capability>,
    /// `shadow` — the decision is made and recorded, and nothing happens.
    pub records_only: Vec<Capability>,
    /// `off` — refused at the endpoint.
    pub withheld: Vec<Capability>,
}

impl Permissions {
    /// Read the charter as the server would.
    ///
    /// A capability with **no row reads `Off`**, matching
    /// `Store::charter_entry`. The notice must not be the one place in the
    /// system where silence reads as permission.
    pub fn read(charter: &[CharterEntry]) -> Self {
        let mut permissions = Self::default();
        for capability in Capability::BY_CONSEQUENCE {
            let level = charter
                .iter()
                .find(|entry| entry.capability == capability)
                .map_or(CharterLevel::Off, |entry| entry.level);
            match level {
                CharterLevel::Live => permissions.on_its_own.push(capability),
                CharterLevel::Shadow => permissions.records_only.push(capability),
                CharterLevel::Off => permissions.withheld.push(capability),
            }
        }
        permissions
    }

    /// The headline: the irreversible things this charter lets it do on its
    /// own, in the person's terms.
    ///
    /// `None` when there are none — an invented warning is worse than no
    /// headline, because the next one is read the same way.
    pub fn sharp_summary(&self) -> Option<String> {
        let sharp: Vec<&'static str> = self
            .on_its_own
            .iter()
            .filter(|capability| capability.is_sharp())
            .map(|capability| capability.consequence())
            .collect();
        if sharp.is_empty() {
            return None;
        }
        Some(format!(
            "As the charter stands, it will also do these without asking, and you cannot \
             take them back: {}.",
            sharp.join("; ")
        ))
    }
}

/// The notice as data, so what it says is testable without a window.
pub struct Notice {
    /// [`disclaimer::HEADLINE`], quoted rather than rewritten.
    pub headline: &'static str,
    /// The unconditional acts on this machine and on the bill.
    pub always: &'static [&'static str],
    /// [`disclaimer::PIPELINE_CAUTION`], the server's own GitHub acts, quoted.
    pub caution: &'static str,
    pub sharp: Option<String>,
    pub permissions: Permissions,
    pub off_switches: &'static [OffSwitch],
    pub pointer: &'static str,
}

impl Notice {
    pub fn build(charter: &[CharterEntry]) -> Self {
        let permissions = Permissions::read(charter);
        Self {
            headline: disclaimer::HEADLINE,
            always: ALWAYS,
            caution: disclaimer::PIPELINE_CAUTION,
            sharp: permissions.sharp_summary(),
            permissions,
            off_switches: OFF_SWITCHES,
            pointer: disclaimer::README_POINTER,
        }
    }
}

/// The one guard every path that sets the mode calls.
///
/// Returns `true` when the press has been intercepted — the caller must do
/// nothing further, because the notice's own button will carry the press
/// through. Returns `false` (carry on) for:
///
/// - any mode but [`Mode::Play`] — pausing and stopping need no explanation;
/// - an install that has already acknowledged;
/// - an answer nobody has yet (an unreachable server, a version too old to
///   have the route). **Absence of evidence must not fire the notice**: the
///   tempting reading of "unknown" is "probably never told, so show it", and
///   that turns an unreachable endpoint into a modal on every press;
/// - a platform that refuses to open the window, because a press that is
///   neither carried out nor explained is a press that vanished.
///
/// There is one of it because four copies of "has this person been told" is
/// how three of them come to disagree. A fifth mode path is easy to add by
/// calling [`AppState::set_mode`] directly; anything new goes through here.
pub fn intercepts(mode: Mode, app_state: &Entity<AppState>, cx: &mut App) -> bool {
    if !wants_notice(mode, app_state.read(cx).autonomy_acknowledged) {
        return false;
    }
    // The platform refusing the window is the last "carry on": a press that
    // is neither carried out nor explained is a press that vanished.
    open(app_state.clone(), true, cx)
}

/// The half of [`intercepts`] that is a rule rather than an effect, split out
/// so it is testable without a gpui `App`.
///
/// Both conditions, and neither is redundant: pausing needs no explanation,
/// and neither does a press on an install that has already been told — nor
/// one where nobody knows, which is the reading that turns an unreachable
/// server into a modal on every press.
pub fn wants_notice(mode: Mode, acknowledged: Option<Option<DateTime<Utc>>>) -> bool {
    mode == Mode::Play && crate::state::owes_autonomy_notice(acknowledged)
}

/// The notice window is a singleton: a second press raises the one that is
/// open rather than stacking another, and a menu-opened window is *upgraded*
/// by a press rather than duplicated.
struct NoticeWindow(WindowHandle<AutonomyNotice>);

impl Global for NoticeWindow {}

/// Open (or raise) the notice. `then_play` is whether an intercepted Play
/// press is riding on it.
///
/// Returns whether the window is up. `false` means the platform refused, and
/// every caller reads that as "carry on without me".
pub fn open(app_state: Entity<AppState>, then_play: bool, cx: &mut App) -> bool {
    if let Some(existing) = cx.try_global::<NoticeWindow>().map(|global| global.0) {
        let raised = existing
            .update(cx, |notice, window, cx| {
                // A press arriving at a window someone opened from the menu
                // upgrades it: acknowledging now starts the pipeline, which
                // is what the person just asked for. Never the reverse — a
                // menu open must not silently inherit a stale press.
                notice.then_play |= then_play;
                window.activate_window();
                cx.notify();
            })
            .is_ok();
        if raised {
            cx.activate(true);
            return true;
        }
    }

    let options = WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some("What Play Does".into()),
            appears_transparent: false,
            traffic_light_position: None,
        }),
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(560.), px(620.)),
            cx,
        ))),
        is_minimizable: false,
        ..Default::default()
    };

    match cx.open_window(options, |_window, cx| {
        cx.new(|_cx| AutonomyNotice {
            app_state,
            then_play,
        })
    }) {
        Ok(handle) => {
            cx.set_global(NoticeWindow(handle));
            cx.activate(true);
            true
        }
        Err(error) => {
            eprintln!("failed to open the autonomy notice: {error}");
            false
        }
    }
}

/// The menu item's entry point: no press is riding, so acknowledging closes
/// the window and starts nothing.
pub fn open_from_menu(cx: &mut App) {
    if let Some(app_state) = crate::state::global(cx) {
        open(app_state, false, cx);
    }
}

pub struct AutonomyNotice {
    app_state: Entity<AppState>,
    /// A Play press was intercepted to show this, and the primary button
    /// carries it through — so nobody presses Play twice.
    then_play: bool,
}

impl AutonomyNotice {
    /// Acknowledge, close, and carry the intercepted press through.
    ///
    /// The window closes **before** the POST settles, and a failure lands in
    /// the sidebar banner: an acknowledgement that did not stick shows the
    /// notice again on the next press, which is the right direction for this
    /// to fail in. The alternative — waiting — would leave a pipeline running
    /// against a record that says nobody was ever told.
    fn acknowledge(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let then_play = self.then_play;
        self.app_state.update(cx, |state, cx| {
            state.acknowledge_autonomy_notice(cx);
            if then_play {
                state.set_mode(Mode::Play, cx);
            }
        });
        window.remove_window();
    }
}

impl Render for AutonomyNotice {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let notice = Notice::build(&self.app_state.read(cx).charter);

        let heading = |text: &'static str| {
            div()
                .mt(px(10.))
                .text_xs()
                .text_color(theme.fg_muted())
                .child(text)
        };
        let bullet = |text: String| {
            div()
                .text_sm()
                .text_color(theme.fg())
                .child(format!("• {text}"))
        };
        let capability_lines = |capabilities: &[Capability]| {
            capabilities
                .iter()
                .map(|capability| bullet(capability.consequence().to_string()))
                .collect::<Vec<_>>()
        };

        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(div().text_color(theme.fg()).child(notice.headline))
            .child(heading("Pressing Play will, on its own:"))
            .children(notice.always.iter().map(|act| bullet(act.to_string())))
            .child(
                div()
                    .mt(px(6.))
                    .text_sm()
                    .text_color(theme.fg())
                    .child(notice.caution),
            );

        if let Some(sharp) = &notice.sharp {
            body = body.child(
                div()
                    .mt(px(6.))
                    .text_sm()
                    .text_color(theme.fg())
                    .child(sharp.clone()),
            );
        }

        if !notice.permissions.on_its_own.is_empty() {
            body = body
                .child(heading("It may do these without asking:"))
                .children(capability_lines(&notice.permissions.on_its_own));
        }
        if !notice.permissions.records_only.is_empty() {
            body = body
                .child(heading(
                    "It will decide these and record the decision, and nothing will happen:",
                ))
                .children(capability_lines(&notice.permissions.records_only));
        }
        if !notice.permissions.withheld.is_empty() {
            body = body
                .child(heading("It may not:"))
                .children(capability_lines(&notice.permissions.withheld));
        }

        body =
            body.child(heading("To stop it:"))
                .children(notice.off_switches.iter().map(|switch| {
                    bullet(format!("{} — {}", off_switch_label(switch), switch.effect))
                }))
                .child(
                    div()
                        .mt(px(10.))
                        .text_xs()
                        .text_color(theme.fg_muted())
                        .child(notice.pointer),
                );

        let button = |id: &'static str, label: String, primary: bool| {
            div()
                .id(id)
                .px(px(10.))
                .py(px(4.))
                .rounded(px(5.))
                .border_1()
                .border_color(theme.border_secondary())
                .text_sm()
                .text_color(match primary {
                    true => theme.fg(),
                    false => theme.fg_muted(),
                })
                .when(primary, |el| el.bg(theme.surface_tertiary()))
                .cursor_pointer()
                .child(label)
        };

        let primary_label = match self.then_play {
            true => "I understand — start it".to_string(),
            false => "I understand".to_string(),
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .p(px(20.))
            .gap(px(8.))
            .bg(theme.bg())
            .font_family(crate::workspace::FONT)
            // The body scrolls: a shadow-heavy charter renders all three
            // lists, and whether 560x620 fits the longest one is not
            // checkable off a Mac.
            .child(
                div()
                    .id("autonomy-body")
                    .flex_1()
                    .overflow_y_scroll()
                    .child(body),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.))
                    .child(
                        button("autonomy-charter", "Open the charter…".to_string(), false)
                            .on_click(cx.listener(|_this, _event, _window, cx| {
                                crate::charter_window::open(cx);
                            })),
                    )
                    .child(button("autonomy-ack", primary_label, true).on_click(
                        cx.listener(|this, _event, window, cx| this.acknowledge(window, cx)),
                    )),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn charter(level: CharterLevel) -> Vec<CharterEntry> {
        Capability::ALL
            .iter()
            .map(|capability| CharterEntry {
                capability: *capability,
                level,
                daily_limit: None,
                updated_at: Utc::now(),
            })
            .collect()
    }

    /// The charter ships all-`live`, so this is the notice almost everybody
    /// reads.
    #[test]
    fn a_live_charter_puts_every_capability_in_the_can_bucket() {
        let permissions = Permissions::read(&charter(CharterLevel::Live));
        assert_eq!(permissions.on_its_own.len(), Capability::ALL.len());
        assert!(permissions.records_only.is_empty());
        assert!(permissions.withheld.is_empty());
    }

    /// Shadow is its own bucket. Collapsing it either way lies: into "can" it
    /// claims an effect that never happens, into "cannot" it hides a judgment
    /// still being made and recorded.
    #[test]
    fn shadow_is_neither_can_nor_cannot() {
        let permissions = Permissions::read(&charter(CharterLevel::Shadow));
        assert!(permissions.on_its_own.is_empty());
        assert_eq!(permissions.records_only.len(), Capability::ALL.len());
        assert!(permissions.withheld.is_empty());
    }

    /// Matching `Store::charter_entry`, which reads a missing row as `off`.
    /// The notice must not be the one place silence reads as permission.
    #[test]
    fn a_capability_with_no_row_is_withheld() {
        let permissions = Permissions::read(&[]);
        assert_eq!(permissions.withheld.len(), Capability::ALL.len());
        assert!(permissions.on_its_own.is_empty());
        assert!(permissions.records_only.is_empty());
    }

    /// Each bucket leads with the sharpest thing in it.
    #[test]
    fn each_bucket_is_in_consequence_order() {
        let permissions = Permissions::read(&charter(CharterLevel::Live));
        assert_eq!(permissions.on_its_own, Capability::BY_CONSEQUENCE.to_vec());
    }

    /// The headline names the irreversible ones and nothing else.
    #[test]
    fn the_summary_names_only_the_sharp_live_capabilities() {
        let permissions = Permissions::read(&charter(CharterLevel::Live));
        let summary = permissions.sharp_summary().expect("live charter is sharp");
        assert!(summary.contains(Capability::LandBuilds.consequence()));
        assert!(summary.contains(Capability::RetireWork.consequence()));
        // Not sharp: reversible, so it belongs in the list of what it may do
        // rather than in the sentence about what cannot be taken back.
        assert!(!summary.contains(Capability::CaptureWork.consequence()));
    }

    /// No invented warning. A charter with nothing sharp live gets no
    /// headline sentence at all — the next one is read the same way as the
    /// last, and one that cried wolf is not read.
    #[test]
    fn a_charter_with_nothing_sharp_gets_no_headline() {
        let entries: Vec<CharterEntry> = Capability::ALL
            .iter()
            .map(|capability| CharterEntry {
                capability: *capability,
                level: match capability.is_sharp() {
                    true => CharterLevel::Off,
                    false => CharterLevel::Live,
                },
                daily_limit: None,
                updated_at: Utc::now(),
            })
            .collect();
        assert!(Permissions::read(&entries).sharp_summary().is_none());
    }

    /// One vocabulary, not two: the static claims are `disclaimer`'s, quoted
    /// rather than rewritten (#984). If someone writes a second headline here
    /// the app has two modules describing the same risk in different words,
    /// which is the failure `disclaimer.rs` exists to prevent.
    #[test]
    fn the_notice_quotes_the_disclaimer_rather_than_restating_it() {
        let notice = Notice::build(&charter(CharterLevel::Live));
        assert_eq!(notice.headline, disclaimer::HEADLINE);
        assert_eq!(notice.caution, disclaimer::PIPELINE_CAUTION);
        assert_eq!(notice.pointer, disclaimer::README_POINTER);
    }

    /// ...and the generated half does not restate the quoted half. `ALWAYS`
    /// carries what happens to the machine and the bill; the caution carries
    /// the server's GitHub acts. One claim, one source, in both directions.
    #[test]
    fn always_says_what_the_disclaimer_does_not() {
        let always = ALWAYS.join(" ").to_lowercase();
        assert!(always.contains("virtual machines"));
        assert!(always.contains("api credit"));
        for verb in ["pull request", "merge", "close"] {
            assert!(
                !always.contains(verb),
                "ALWAYS restates the caution's {verb}: {always}"
            );
        }
    }

    /// The notice points people at menu items by name, so a rename must move
    /// the text rather than leave it pointing at something that is not there.
    /// Driven off the array itself, so a fifth off switch is covered without
    /// anyone remembering to add it here.
    #[test]
    fn every_off_switch_the_notice_names_is_in_the_server_menu() {
        for switch in OFF_SWITCHES {
            let command = crate::commands::by_id(switch.command)
                .unwrap_or_else(|| panic!("no command with id {}", switch.command));
            assert_eq!(
                command.menu,
                Some(crate::commands::Slot::Server),
                "{} is not in the Server menu",
                switch.command
            );
            assert!(off_switch_label(switch).starts_with("Server ▸ "));
            assert!(!switch.effect.trim().is_empty());
        }
    }

    /// Only Play is explained. Pausing and stopping are the off switches the
    /// notice itself points at — intercepting one would be absurd.
    #[test]
    fn only_play_is_ever_intercepted() {
        for mode in [Mode::Pause, Mode::Stop] {
            assert!(!wants_notice(mode, Some(None)), "{mode:?}");
        }
        assert!(wants_notice(Mode::Play, Some(None)));
    }

    /// The two ways a press carries on: already told, and nobody knows.
    #[test]
    fn a_play_press_carries_on_once_told_and_while_unknown() {
        assert!(!wants_notice(Mode::Play, Some(Some(Utc::now()))));
        assert!(!wants_notice(Mode::Play, None));
    }
}
