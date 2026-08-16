//! The Server window: what is serving, and the operations that change it.
//!
//! A restart is minutes of staged work — build, report, gate, drain, swap,
//! verify — so it gets a window rather than a menu item that returns in
//! silence. The top half is the same report `tasks status` prints; the bottom
//! half streams the child's output line by line and ends on the verdict its
//! exit code earned.
//!
//! A refusal is the one outcome that grows buttons. `tasks reload` can only
//! refuse and exit; a GUI can ask — wait for a drain point, or swap anyway —
//! which is most of what this window is for.
//!
//! It polls `/status` while it is open, and only then: a stopped server
//! publishes no events to refresh on, and a stopped server is a state this
//! window exists to produce.

use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    div, px, size, App, Bounds, ClickEvent, Context, Entity, Global, Hsla, TitlebarOptions, Window,
    WindowBounds, WindowHandle, WindowOptions,
};
use gpuikit::theme::{ActiveTheme, Themeable};
use tasks_client::api::http::{InFlight, ServerStatus};
use tasks_client::api::models::Mode;

use crate::about;
use crate::server::{Op, ServerControl};
use crate::time;
use crate::workspace::FONT;

/// How often `/status` is re-read while the window is open. Loopback is
/// sub-millisecond; the cost is a log line on the server and nothing else.
///
/// Public because it is the age of the answer the Stop confirmation is raised
/// from: that prompt is a courtesy with this much staleness in it, not a lock.
pub const POLL: Duration = Duration::from_secs(5);

/// The window is a singleton: a second "Server Status…" raises the one that
/// is already open rather than stacking another.
struct ServerWindowHandle(WindowHandle<ServerWindow>);

impl Global for ServerWindowHandle {}

pub struct ServerWindow {
    control: Entity<ServerControl>,
}

/// Open the window, or raise it if it is already open.
pub fn open(cx: &mut App) {
    let control = ServerControl::global(cx);
    // A fresh read on every open: the window's whole top half is a claim
    // about right now.
    control.update(cx, |control, cx| control.refresh(cx));

    // A `WindowHandle` stays structurally valid after its window closes, so
    // the only way to tell a stale one apart is that `update` fails.
    if let Some(existing) = cx.try_global::<ServerWindowHandle>().map(|global| global.0) {
        if existing
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
        {
            cx.activate(true);
            return;
        }
    }

    let options = WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some("Server".into()),
            appears_transparent: false,
            traffic_light_position: None,
        }),
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(620.), px(600.)),
            cx,
        ))),
        ..Default::default()
    };

    match cx.open_window(options, |window, cx| {
        cx.new(|cx| ServerWindow::new(control, window, cx))
    }) {
        Ok(handle) => {
            cx.set_global(ServerWindowHandle(handle));
            cx.activate(true);
        }
        // Not worth taking the app down over; the menu item just does nothing.
        Err(error) => eprintln!("failed to open the Server window: {error}"),
    }
}

/// Open the window *and* ask for `op`. Both, always: a menu item that starts
/// minutes of staged work silently is a spinner that resolves to nothing.
///
/// `request` rather than `start`, so an immediate Stop with work in flight
/// arrives here as a question in the window it just opened rather than as a
/// process that is already gone.
pub fn run(cx: &mut App, op: Op) {
    open(cx);
    let control = ServerControl::global(cx);
    control.update(cx, |control, cx| {
        control.request(op, cx);
    });
}

impl ServerWindow {
    fn new(control: Entity<ServerControl>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&control, |_, _, cx| cx.notify()).detach();

        // One second, because the run's clock lives here; `/status` every
        // fifth tick, because that is a probe and not a clock. Both stop when
        // the window closes, which is the point — nothing polls a server the
        // user is not looking at.
        let executor = cx.background_executor().clone();
        let polled = control.clone();
        cx.spawn(async move |this, cx| {
            let mut ticks: u64 = 0;
            loop {
                executor.timer(Duration::from_secs(1)).await;
                ticks += 1;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
                if ticks.is_multiple_of(POLL.as_secs()) {
                    polled.update(cx, |control, cx| control.refresh(cx));
                }
            }
        })
        .detach();

        Self { control }
    }

    // --- the report ---

    fn render_facts(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let control = self.control.read(cx);
        let status = control.status.clone();
        let version = control.version.clone();
        let probe_error = control.probe_error.clone();
        let data_dir = control
            .data_dir
            .as_ref()
            .map(|dir| dir.display().to_string())
            .unwrap_or_else(|| "unknown ($HOME is not set)".to_string());

        // The freshness of the answer belongs next to the answer: this is a
        // poll, so "not serving" is a claim with an age on it.
        let checked = control
            .probed_at
            .map(|at| format!("  ·  checked {} ago", time::since(at)))
            .unwrap_or_default();
        let serving = match &status {
            Some(status) => format!(
                "pid {}  ·  up {}{checked}",
                status.pid,
                time::since(status.started_at)
            ),
            None => match &probe_error {
                Some(err) => format!("not serving{checked}  ({err})"),
                None => format!("not serving{checked}"),
            },
        };
        let mode = status
            .as_ref()
            .map(|status| status.mode.as_str().to_string())
            .unwrap_or_else(|| "—".to_string());
        let migrations = status
            .as_ref()
            .map(migrations_line)
            .unwrap_or_else(|| "—".to_string());
        let in_flight = status
            .as_ref()
            .map(in_flight_lines)
            .unwrap_or_else(|| "—".to_string());
        let server_build = match &version {
            Some(version) => format!("{}  ({})", version.version, version.commit),
            // The server answered `/status` but not `/version`: it predates
            // the route, which makes it the stale end of the pair.
            None if status.is_some() => "unversioned (predates /version)".to_string(),
            None => "—".to_string(),
        };

        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(self.fact("Server", serving, cx))
            .child(self.fact("Pipeline", mode, cx))
            .child(self.fact("Migrations", migrations, cx))
            .child(self.fact("In flight", in_flight, cx))
            .child(self.fact("Server build", server_build, cx))
            .child(self.fact(
                "App build",
                format!("{}  ({})", about::VERSION, about::COMMIT),
                cx,
            ))
            .child(self.fact("Data dir", data_dir, cx))
    }

    fn fact(&self, label: &'static str, value: String, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        div()
            .flex()
            .flex_row()
            .items_start()
            .gap(px(10.))
            .text_xs()
            .child(
                div()
                    .flex_none()
                    .w(px(96.))
                    .text_color(theme.fg_muted())
                    .child(label),
            )
            .child(div().flex_1().text_color(theme.fg()).child(value))
    }

    // --- the operations ---

    fn render_actions(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let control = self.control.read(cx);
        let busy = control.busy();
        let serving = control.serving();
        let restart_label = match serving {
            true => "Restart",
            false => "Start",
        };

        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(6.))
            .child(self.button(
                "restart",
                restart_label,
                !busy,
                None,
                cx.listener(|this, _event: &ClickEvent, _window, cx| {
                    this.start(Op::Restart, cx);
                }),
                cx,
            ))
            .child(self.button(
                "restart-when-idle",
                "Restart When Idle",
                !busy && serving,
                None,
                cx.listener(|this, _event: &ClickEvent, _window, cx| {
                    this.start(Op::RestartWhenIdle, cx);
                }),
                cx,
            ))
            // The same pair as above, the same way round: the immediate verb
            // and the patient one beside it.
            .child(self.button(
                "stop",
                "Stop",
                !busy && serving,
                Some(gpui::hsla(0. / 360., 0.8, 0.62, 1.)),
                // Through `request`: with work in flight this parks the
                // question below rather than ending the process under it.
                cx.listener(|this, _event: &ClickEvent, _window, cx| {
                    this.control.update(cx, |control, cx| {
                        control.request(Op::Stop, cx);
                    });
                }),
                cx,
            ))
            .child(self.button(
                "stop-when-idle",
                "Stop When Idle",
                !busy && serving,
                None,
                cx.listener(|this, _event: &ClickEvent, _window, cx| {
                    this.start(Op::StopWhenIdle, cx);
                }),
                cx,
            ))
            .child(self.button(
                "refresh-status",
                "Refresh",
                true,
                None,
                cx.listener(|this, _event: &ClickEvent, _window, cx| {
                    this.control.update(cx, |control, cx| control.refresh(cx));
                }),
                cx,
            ))
    }

    /// The pipeline mode, as a row of three. Here as well as in the menu
    /// because this window is the one place the pipeline can be reached with
    /// no workspace focused — and because "paused" is half the explanation
    /// for a quiet server.
    fn render_pipeline(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let control = self.control.read(cx);
        let current = control.mode();
        let serving = control.serving();
        let error = control.mode_error.clone();
        let theme = cx.theme().clone();

        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex_none()
                            .w(px(96.))
                            .text_xs()
                            .text_color(theme.fg_muted())
                            .child("Pipeline"),
                    )
                    .children([Mode::Play, Mode::Pause, Mode::Stop].map(|mode| {
                        let selected = current == Some(mode);
                        self.mode_button(mode, selected, serving, cx)
                    })),
            )
            .when_some(error, |el, error| {
                el.child(
                    div()
                        .pl(px(102.))
                        .text_xs()
                        .text_color(gpui::hsla(30. / 360., 0.9, 0.6, 1.))
                        .child(error),
                )
            })
    }

    fn mode_button(
        &self,
        mode: Mode,
        selected: bool,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let label = crate::components::title_case(mode.as_str());
        let base = div()
            .id(gpui::SharedString::from(format!("mode-{}", mode.as_str())))
            .px(px(8.))
            .py(px(3.))
            .rounded(px(5.))
            .border_1()
            .border_color(theme.border_secondary())
            .text_xs();
        if !enabled {
            return base
                .text_color(theme.fg_muted())
                .opacity(0.5)
                .child(label)
                .into_any_element();
        }
        base.when(selected, |el| el.bg(theme.surface_tertiary()))
            .text_color(match selected {
                true => theme.fg(),
                false => theme.fg_muted(),
            })
            .cursor_pointer()
            .hover({
                let hover_bg = theme.surface_secondary();
                move |el| el.bg(hover_bg)
            })
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.control
                    .update(cx, |control, cx| control.set_mode(mode, cx));
            }))
            .child(label)
            .into_any_element()
    }

    fn button(
        &self,
        id: &'static str,
        label: &'static str,
        enabled: bool,
        color: Option<Hsla>,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let base = div()
            .id(id)
            .px(px(8.))
            .py(px(3.))
            .rounded(px(5.))
            .border_1()
            .border_color(theme.border_secondary())
            .text_xs();
        if !enabled {
            return base
                .text_color(theme.fg_muted())
                .opacity(0.5)
                .child(label)
                .into_any_element();
        }
        base.text_color(color.unwrap_or_else(|| theme.fg()))
            .cursor_pointer()
            .hover({
                let hover_bg = theme.surface_secondary();
                move |el| el.bg(hover_bg)
            })
            .on_click(on_click)
            .child(label)
            .into_any_element()
    }

    fn start(&mut self, op: Op, cx: &mut Context<Self>) {
        self.control.update(cx, |control, cx| {
            control.start(op, cx);
        });
    }

    // --- the run ---

    fn render_run(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let control = self.control.read(cx);
        let Some(run) = control.run.as_ref() else {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_xs()
                .text_color(theme.fg_muted())
                .child("No restart has been run from here yet.")
                .into_any_element();
        };

        let op = run.op;
        let outcome = run.outcome;
        // A clock while it runs, because a restart is minutes long and a
        // clock reads as working where a spinner reads as hung.
        let heading = match outcome {
            None => format!("{}…  {}", op.label(), time::elapsed(run.started_at)),
            // How long it took, kept next to the verdict: a swap that took
            // four minutes and one that took four seconds are different
            // events, and only one of them built anything.
            Some(outcome) => match run.finished_at {
                Some(finished) => format!(
                    "{}  ({})",
                    outcome.headline(op),
                    time::duration(finished - run.started_at)
                ),
                None => outcome.headline(op),
            },
        };
        let heading_color = match outcome {
            None => theme.fg_muted(),
            Some(outcome) if outcome.is_success() => theme.fg(),
            Some(_) => gpui::hsla(30. / 360., 0.9, 0.6, 1.),
        };
        let lines: Vec<String> = run.lines.iter().cloned().collect();

        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(heading_color)
                    .child(heading),
            )
            .child(
                div()
                    .id("server-log")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .p(px(8.))
                    .rounded(px(6.))
                    .bg(theme.surface())
                    .text_xs()
                    .text_color(theme.fg_muted())
                    .children(lines.into_iter().map(|line| div().child(line))),
            )
            .children(
                outcome
                    .filter(|outcome| outcome.is_refusal())
                    .map(|_| self.render_refusal(op, cx)),
            )
            .into_any_element()
    }

    /// The fork a refusal earns. The CLI could only refuse and exit; the two
    /// ways forward it names in prose are buttons here.
    ///
    /// Which two depends on what was refused. A restart's exit 3 is "work in
    /// flight"; a stop's is "the server will not say what is in flight", which
    /// makes waiting a retry rather than a plan — so the wording forks.
    ///
    /// Only the two ungated ops ever reach this: `--when-idle` skips the
    /// refusal branch entirely, and its failure is a drain timeout.
    fn render_refusal(&self, op: Op, cx: &mut Context<Self>) -> gpui::AnyElement {
        let busy = self.control.read(cx).busy();
        let (wait_id, wait_label, wait_op) = match op.stops() {
            true => ("stop-try-again", "Try again", Op::StopWhenIdle),
            false => (
                "wait-then-restart",
                "Wait, then restart",
                Op::RestartWhenIdle,
            ),
        };
        let (anyway_id, anyway_label, anyway_op) = match op.stops() {
            true => ("stop-anyway", "Stop anyway", Op::Stop),
            false => ("restart-anyway", "Restart anyway", Op::RestartAnyway),
        };
        div()
            .flex_none()
            .flex()
            .flex_row()
            .gap(px(6.))
            .child(self.button(
                wait_id,
                wait_label,
                !busy,
                None,
                cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                    this.start(wait_op, cx);
                }),
                cx,
            ))
            .child(self.button(
                anyway_id,
                anyway_label,
                !busy,
                Some(gpui::hsla(0. / 360., 0.8, 0.62, 1.)),
                cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                    this.start(anyway_op, cx);
                }),
                cx,
            ))
            .into_any_element()
    }

    // --- the parked question ---

    /// The question an immediate Stop with work in flight parks instead of
    /// answering itself.
    ///
    /// The CLI cannot ask — plain `tasks stop` is what scripts and the reload
    /// path rely on, so it stays immediate — but the window has been polling
    /// `/status` all along, so it already knows what is about to be ended.
    /// Both halves of the trade are stated, and Cancel is a real answer.
    ///
    /// It is raised from a poll, so it is up to [`POLL`] stale: a courtesy,
    /// not a lock. If the work lands while it is up the question collapses
    /// (see `ServerControl::refresh`) and the click is dropped — stopping on a
    /// question nobody is being asked any more would be worse.
    fn render_confirm(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let control = self.control.read(cx);
        let op = control.pending?;
        let work = work_lines(control.destructible()?);
        let theme = cx.theme().clone();

        Some(
            div()
                .flex_none()
                .flex()
                .flex_col()
                .gap(px(6.))
                .p(px(8.))
                .rounded(px(6.))
                .bg(theme.surface())
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.fg())
                        .child(format!("Stop now, with {work} in flight?")),
                )
                .child(div().text_xs().text_color(theme.fg_muted()).child(
                    "Stopping now leaves that work's VMs running with nothing \
                             reading them until vm-pool reaps them. Waiting stops the \
                             server once it lands — and leaves dispatch paused, since \
                             no boot resumes it.",
                ))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(6.))
                        .child(self.button(
                            "confirm-wait-then-stop",
                            "Wait, then stop",
                            true,
                            None,
                            cx.listener(|this, _event: &ClickEvent, _window, cx| {
                                this.start(Op::StopWhenIdle, cx);
                            }),
                            cx,
                        ))
                        .child(self.button(
                            "confirm-stop-anyway",
                            "Stop anyway",
                            true,
                            Some(gpui::hsla(0. / 360., 0.8, 0.62, 1.)),
                            cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                                this.start(op, cx);
                            }),
                            cx,
                        ))
                        .child(self.button(
                            "confirm-cancel",
                            "Cancel",
                            true,
                            None,
                            cx.listener(|this, _event: &ClickEvent, _window, cx| {
                                this.control
                                    .update(cx, |control, cx| control.cancel_pending(cx));
                            }),
                            cx,
                        )),
                )
                .into_any_element(),
        )
    }
}

impl Render for ServerWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let facts = self.render_facts(cx);
        let actions = self.render_actions(cx);
        let pipeline = self.render_pipeline(cx);
        let confirm = self.render_confirm(cx);
        let run = self.render_run(cx);

        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .size_full()
            .p(px(12.))
            .bg(theme.bg())
            .font_family(FONT)
            .text_color(theme.fg())
            .child(facts)
            .child(actions)
            .child(pipeline)
            .children(confirm)
            .child(
                div()
                    .flex_none()
                    .h(px(1.))
                    .w_full()
                    .bg(theme.border_subtle()),
            )
            .child(run)
    }
}

/// What that boot did to the schema — the same sentence `tasks reload` prints.
fn migrations_line(status: &ServerStatus) -> String {
    match status.migrations_applied.as_slice() {
        [] => "already current".to_string(),
        applied => applied
            .iter()
            .map(|migration| migration.file_stem())
            .collect::<Vec<_>>()
            .join(", "),
    }
}

/// Work a restart would destroy, with ages — the thing you are about to
/// interrupt, named.
fn in_flight_lines(status: &ServerStatus) -> String {
    work_lines(&status.in_flight)
}

/// The same list, off the [`InFlight`] alone — the Stop confirmation names
/// what it is about to end, and it must be the same sentence the facts above
/// it are showing.
fn work_lines(in_flight: &InFlight) -> String {
    if in_flight.is_empty() {
        return "nothing".to_string();
    }
    let mut parts = Vec::new();
    for item in &in_flight.scouts {
        parts.push(format!("scout {} ({})", item.id, time::since(item.since)));
    }
    for item in &in_flight.builds {
        parts.push(format!("build {} ({})", item.id, time::since(item.since)));
    }
    if let Some(item) = &in_flight.orchestrator {
        // Reported, never a reason to wait: the answered watermark means a
        // restart mid-turn costs one turn and the next boot takes it again.
        parts.push(format!(
            "owed turn {} ({})",
            item.id,
            time::since(item.since)
        ));
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use tasks_client::api::http::{AppliedMigration, InFlight, InFlightItem};

    fn status() -> ServerStatus {
        ServerStatus {
            pid: 4242,
            started_at: Utc::now(),
            migrations_applied: Vec::new(),
            mode: Mode::Play,
            in_flight: InFlight::default(),
        }
    }

    #[test]
    fn a_boot_that_moved_the_schema_names_the_migrations() {
        let mut status = status();
        assert_eq!(migrations_line(&status), "already current");
        status.migrations_applied = vec![AppliedMigration {
            version: 19,
            description: "charter comment and land".into(),
        }];
        assert_eq!(migrations_line(&status), "0019_charter_comment_and_land");
    }

    #[test]
    fn in_flight_work_is_named_and_aged() {
        let mut status = status();
        assert_eq!(in_flight_lines(&status), "nothing");
        status.in_flight = InFlight {
            scouts: vec![InFlightItem {
                id: "sess_1".into(),
                detail: None,
                since: Utc::now(),
            }],
            builds: Vec::new(),
            orchestrator: Some(InFlightItem {
                id: "17".into(),
                detail: None,
                since: Utc::now(),
            }),
        };
        let line = in_flight_lines(&status);
        assert!(line.contains("scout sess_1"), "{line}");
        assert!(line.contains("owed turn 17"), "{line}");
    }
}
