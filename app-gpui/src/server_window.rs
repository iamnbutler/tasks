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
    div, px, size, App, Bounds, ClickEvent, Context, Entity, FocusHandle, Global, Hsla,
    TitlebarOptions, Window, WindowBounds, WindowHandle, WindowOptions,
};
use gpuikit::theme::{ActiveTheme, Themeable};
use tasks_client::api::http::{InFlight, ServerStatus, VerifyDirTier};
use tasks_client::api::models::{CharterLevel, Mode};

use crate::about;
use crate::modal::{self, modal, Dismissal, ModalLayer, Placement, Scrim};
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

/// This window's seat in the modal layer. One name, because a window that
/// wants to ask two things at once is asking one of them of nobody.
const CONFIRM_MODAL: &str = "Stop confirmation";

pub struct ServerWindow {
    control: Entity<ServerControl>,
    /// Where focus rests when no modal is up, and what a dismissed modal
    /// hands focus back to when nothing else held it.
    focus_handle: FocusHandle,
    modals: ModalLayer,
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

        let focus_handle = cx.focus_handle();
        Self {
            control,
            modals: ModalLayer::new(focus_handle.clone()),
            focus_handle,
        }
    }

    /// Keep the modal layer in step with the question the *control* is
    /// holding.
    ///
    /// `ServerControl::pending` stays the single source of truth for whether
    /// there is anything to ask — it is set from a menu item this window never
    /// sees, and collapsed from a poll when the work it was about lands
    /// (`ServerControl::refresh`). So the modal is opened and closed from that
    /// one fact rather than from each of the four things that can change it:
    /// the three buttons and the poll would otherwise be four places to
    /// remember to put the surface away.
    fn sync_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let asking = self.control.read(cx).pending.is_some();
        if asking && !self.modals.is_open(CONFIRM_MODAL) {
            if let Err(conflict) = self.modals.open(CONFIRM_MODAL, None, window, cx) {
                // Nothing else in this window opens a modal, so this is a
                // future bug rather than a state to handle — said out loud
                // where the window's other refusals are said.
                eprintln!("{conflict}");
            }
        } else if !asking && self.modals.is_open(CONFIRM_MODAL) {
            self.modals.dismiss(window, cx);
        }
    }

    /// Drop the parked question. Escape, the scrim and the Cancel button all
    /// arrive here — one answer, three gestures.
    fn cancel_pending(&mut self, cx: &mut Context<Self>) {
        self.control
            .update(cx, |control, cx| control.cancel_pending(cx));
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
        let images = status
            .as_ref()
            .map(images_line)
            .unwrap_or_else(|| "—".to_string());
        let server_build = match &version {
            Some(version) => format!("{}  ({})", version.version, version.commit),
            // The server answered `/status` but not `/version`: it predates
            // the route, which makes it the stale end of the pair.
            None if status.is_some() => "unversioned (predates /version)".to_string(),
            None => "—".to_string(),
        };

        // Only when there is something to say — see `github_hold_line`.
        let github = status
            .as_ref()
            .and_then(github_hold_line)
            .map(|line| self.fact("GitHub", line, cx));
        let update = status
            .as_ref()
            .and_then(update_pending_line)
            .map(|line| self.fact("Update", line, cx));
        let pool = status
            .as_ref()
            .and_then(pool_hold_line)
            .map(|line| self.fact("vm-pool", line, cx));
        let broker = status
            .as_ref()
            .and_then(broker_hold_line)
            .map(|line| self.fact("Broker", line, cx));
        let runtime = status
            .as_ref()
            .and_then(runtime_hold_line)
            .map(|line| self.fact("Runtime", line, cx));
        // Unlike the three above it, this row is NOT an exception report: it
        // appears whenever there is a reading. See `verify_dir_line`.
        let verify_dir = status
            .as_ref()
            .and_then(verify_dir_line)
            .map(|line| self.fact("Verify dir", line, cx));

        // One row per sealed name, always both — this is the read half of
        // #1005's Credentials surface: what is serving each name, and what
        // sealing or removing it would do. The paste field that would make it
        // writable is not here yet; until it is, the row names the command
        // that seals one, so the reader is never told a fact with no act
        // beside it.
        let credentials: Vec<_> = crate::secrets::rows(self.control.read(cx).secrets.clone().as_ref())
            .into_iter()
            .map(|row| {
                let mut line = row.detail.clone();
                if let Some(nudge) = &row.degraded {
                    line.push_str("  ");
                    line.push_str(nudge);
                    line.push_str(&format!("  (`tasks secrets set {}`)", row.name));
                } else if !row.consequence.is_empty() {
                    line.push_str("  ");
                    line.push_str(&row.consequence);
                }
                self.fact(credential_label(row.name), line, cx)
            })
            .collect();

        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(self.fact("Server", serving, cx))
            .child(self.fact("Pipeline", mode, cx))
            .children(github)
            .children(update)
            .children(pool)
            .children(broker)
            .children(runtime)
            .children(verify_dir)
            .children(credentials)
            .child(self.fact("Migrations", migrations, cx))
            .child(self.fact("In flight", in_flight, cx))
            .child(self.fact("Server build", server_build, cx))
            .child(self.fact("VM images", images, cx))
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
            // Wraps rather than overflowing — see the headline row in
            // `sections/detail.rs`. These thirteen callers render the longest
            // server-written strings in the app: the GitHub hold sentence, each
            // update-hold reason (which names its own discharge command), the
            // broker/vm-pool/runtime hold text, the verify-directory reclaim
            // line and the data-dir path. `items_start()` above is already what
            // you write when you expect a value to wrap.
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_color(theme.fg())
                    .child(value),
            )
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
            // Always, not only while paused: a caution that disappears once
            // the risk is taken only ever warns the people not taking it.
            .child(
                div()
                    .pl(px(102.))
                    .max_w(px(420.))
                    .text_xs()
                    .text_color(theme.fg_muted())
                    .child(crate::disclaimer::PIPELINE_CAUTION),
            )
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
            .on_click(cx.listener(move |this, _event, window, cx| {
                let set = this
                    .control
                    .update(cx, |control, cx| control.set_mode_gated(mode, cx));
                // Refused: this is the first user-initiated `play` on this
                // install. Raise the sheet and change nothing — the confirm
                // button is what writes the acknowledgement and then plays.
                if !set {
                    this.ask_first_play(window, cx);
                }
            }))
            .child(label)
            .into_any_element()
    }

    /// Raise the before-first-`play` sheet (#993).
    ///
    /// A request for it while the Stop confirmation is up is the
    /// [`crate::modal::ModalConflict`] the layer surfaces rather than
    /// resolves; this window reports it into `mode_error`, where the pipeline
    /// row's own errors already land.
    fn ask_first_play(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(conflict) = self
            .modals
            .open(crate::first_play::SHEET_MODAL, None, window, cx)
        {
            self.control.update(cx, |control, cx| {
                control.mode_error = Some(conflict.to_string());
                cx.notify();
            });
        }
        cx.notify();
    }

    /// The sheet: what `play` will do, once per install.
    ///
    /// [`Dismissal::Dismissible`] — escape and the scrim mean "not now" and
    /// start nothing, which is a real answer, and a modal whose safe exit
    /// needs a specific button is one whose other button is the easier target.
    ///
    /// **No `on_submit`**: ⌘-Enter deliberately does nothing here, because a
    /// hand that reflexively hits it has not read the sheet, and this is the
    /// one surface whose whole purpose is that the words get read.
    fn render_first_play(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.modals.is_open(crate::first_play::SHEET_MODAL) {
            return None;
        }
        let theme = cx.theme().clone();
        let charter = self.control.read(cx).charter.clone();
        Some(
            modal(&self.modals)?
                .scrim(Scrim::Dim)
                .placement(Placement::Center)
                .dismissal(Dismissal::Dismissible)
                .on_dismiss(cx, |this, window, cx| {
                    this.modals.dismiss(window, cx);
                    cx.notify();
                })
                .child(
                    modal::panel(cx)
                        .id("first-play")
                        .w(px(520.))
                        .gap(px(10.))
                        .p(px(14.))
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.fg())
                                .child(crate::first_play::TITLE),
                        )
                        .child(crate::first_play::sheet_body(charter.as_deref(), cx))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .gap(px(6.))
                                .child(self.button(
                                    "first-play-start",
                                    "Start the pipeline",
                                    true,
                                    None,
                                    cx.listener(|this, _event: &ClickEvent, window, cx| {
                                        // The acknowledgement is written
                                        // first, and *then* the mode is set:
                                        // an acknowledgement that lands only
                                        // on a successful play would re-ask on
                                        // every failed one.
                                        crate::first_play::FirstPlay::acknowledge(cx);
                                        this.modals.dismiss(window, cx);
                                        this.control.update(cx, |control, cx| {
                                            control.set_mode(Mode::Play, cx)
                                        });
                                    }),
                                    cx,
                                ))
                                .child(self.button(
                                    "first-play-not-now",
                                    "Not now  esc",
                                    true,
                                    None,
                                    cx.listener(|this, _event: &ClickEvent, window, cx| {
                                        this.modals.dismiss(window, cx);
                                        cx.notify();
                                    }),
                                    cx,
                                )),
                        ),
                )
                .into_any_element(),
        )
    }

    /// The charter, read-only, under the pipeline row and the caution (#993).
    ///
    /// This window is already where the off switches and the caution live, so
    /// it is where the thing they switch belongs. The list is most of the
    /// value; per-row level controls are the whole of it, and are the half cut
    /// for time here — the spec says to ship the list rather than a partial
    /// set of toggles.
    ///
    /// `None` is its own state: "the charter could not be read", never eleven
    /// `off` rows.
    fn render_charter(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let charter = self.control.read(cx).charter.clone();
        let sheet = tasks_api::first_play::Sheet::from_charter(charter.as_deref());
        let mut column = div()
            .flex()
            .flex_col()
            .gap(px(3.))
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
                            .child("Charter"),
                    )
                    .child(div().text_xs().text_color(theme.fg_muted()).child(
                        "What the orchestrator may do without being asked.                          POST /charter/{capability} sets a level.",
                    )),
            );
        if sheet.unreadable {
            return column.child(
                div()
                    .pl(px(102.))
                    .max_w(px(460.))
                    .text_xs()
                    .text_color(theme.warning())
                    .child(tasks_api::first_play::UNREADABLE_CHARTER),
            );
        }
        for (level, lines) in [
            (CharterLevel::Live, &sheet.live),
            (CharterLevel::Shadow, &sheet.shadow),
            (CharterLevel::Off, &sheet.off),
        ] {
            for line in lines.iter() {
                let color = match level {
                    CharterLevel::Live => theme.success(),
                    CharterLevel::Shadow => theme.warning(),
                    CharterLevel::Off => theme.fg_muted(),
                };
                column = column.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.))
                        .pl(px(102.))
                        .child(
                            div()
                                .flex_none()
                                .w(px(48.))
                                .text_xs()
                                .text_color(color)
                                .child(level.as_str().to_uppercase()),
                        )
                        .child(
                            div()
                                .max_w(px(400.))
                                .text_xs()
                                .text_color(theme.fg_muted())
                                .child(gpui::SharedString::from(line.permits)),
                        ),
                );
            }
        }
        column
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
    ///
    /// It draws as [`crate::modal`]'s one modal rather than as a panel inline
    /// in the column, which is what makes the two ways *out* of it real: this
    /// used to be three buttons over a window whose every other control stayed
    /// live behind them, so the question could be walked past rather than
    /// answered.
    ///
    /// It is [`Dismissal::Dismissible`] and not [`Dismissal::MustAnswer`],
    /// even though the thing it guards is destructive: Cancel *is* an answer
    /// here, and a modal whose safe exit needs a specific button is one whose
    /// destructive button is the easier target. ⌘-Enter takes the cautious
    /// side, never the one that ends work.
    fn render_confirm(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let control = self.control.read(cx);
        let op = control.pending?;
        let work = work_lines(control.destructible()?);
        let theme = cx.theme().clone();

        Some(
            modal(&self.modals)?
                .scrim(Scrim::Dim)
                .placement(Placement::Center)
                .dismissal(Dismissal::Dismissible)
                .on_dismiss(cx, |this, _window, cx| this.cancel_pending(cx))
                // The default answer, and deliberately the one that ends
                // nothing: whatever ⌘-Enter is on, it is what an impatient
                // hand reaches for.
                .on_submit(cx, |this, _window, cx| {
                    this.start(Op::StopWhenIdle, cx);
                })
                .child(
                    modal::panel(cx)
                        .id("stop-confirm")
                        .w(px(420.))
                        .gap(px(8.))
                        .p(px(12.))
                        .child(
                            div()
                                .text_sm()
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
                                    "Wait, then stop  ⌘↩",
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
                                    "Cancel  esc",
                                    true,
                                    None,
                                    cx.listener(|this, _event: &ClickEvent, _window, cx| {
                                        this.cancel_pending(cx);
                                    }),
                                    cx,
                                )),
                        ),
                )
                .into_any_element(),
        )
    }
}

impl Render for ServerWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The layer follows the parked question, and then holds the focus that
        // question needs to be answerable from the keyboard.
        self.sync_confirm(window, cx);
        self.modals.hold_focus(window, cx);

        let theme = cx.theme().clone();
        let facts = self.render_facts(cx);
        let actions = self.render_actions(cx);
        let pipeline = self.render_pipeline(cx);
        let charter = self.render_charter(cx);
        let confirm = self.render_confirm(cx);
        let first_play = self.render_first_play(cx);
        let run = self.render_run(cx);

        div()
            // An id and a tracked focus handle so this window has somewhere to
            // put focus at rest — which is what a dismissed modal hands it back
            // to, and what keeps a focused element in the frame at all.
            .id("server-window")
            .track_focus(&self.focus_handle)
            // The containing block the modal positions against.
            .relative()
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
            .child(charter)
            .child(
                div()
                    .flex_none()
                    .h(px(1.))
                    .w_full()
                    .bg(theme.border_subtle()),
            )
            .child(run)
            // Last, over everything: the question is not one of the rows of
            // this window any more.
            .children(confirm)
            .children(first_play)
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

/// What the scout and builder VM images are running, and whether that is a
/// problem — the same reading `tasks status` prints.
///
/// **"None observed yet" is not "current".** Nothing polls an image; the only
/// way to learn what is inside one is to run something in it, so a server that
/// has dispatched nothing since booting knows nothing here. Saying "current"
/// would be an answer it does not have.
///
/// Every stale entry names `make images`, because the rebuild is a host-side
/// command — there is nothing in this window to click, and a verdict word
/// alone would tell a reader they have a problem without telling them what to
/// type.
fn images_line(status: &ServerStatus) -> String {
    if status.images.is_empty() {
        return "none observed yet".to_string();
    }
    let mut parts = Vec::new();
    for image in &status.images {
        let identity = match &image.version {
            Some(version) => version.clone(),
            None => "PREDATES STAMPING".to_string(),
        };
        parts.push(format!(
            "{} {} ({})",
            image.image,
            identity,
            image.freshness.as_str()
        ));
    }
    if status.images.iter().any(|i| i.freshness.needs_rebuild()) {
        parts.push("run `make images` on the host".to_string());
    }
    parts.join(", ")
}

/// Why the pipeline is idle, when the reason is that GitHub is not answering —
/// the same reading `tasks status` prints.
///
/// `None` when there is no hold, and the row is left out entirely: a standing
/// "GitHub ok" line is one a reader learns to skip, and this one has to land
/// the one time it appears.
///
/// Both ages are shown. How long the outage has run and how long ago the last
/// observation was are different facts, and the gap between them is the
/// difference between a hold somebody is still refreshing and one about to
/// expire on its own.
/// Why new work is waiting, when the reason is a half-applied upgrade. Same
/// contract as [`github_hold_line`]: `None` renders no row at all.
fn update_pending_line(status: &ServerStatus) -> Option<String> {
    let update = status.update.as_ref()?;
    let effect = match update.enforced {
        true => "new scouts and builds wait",
        false => "reported only — TASKS_UPDATE_HOLD=off",
    };
    Some(format!("pending ({effect}): {}", update.reasons.join("; ")))
}

fn github_hold_line(status: &ServerStatus) -> Option<String> {
    let hold = status.github.as_ref()?;
    Some(format!(
        "not answering for {} ({} failed call(s), last {} ago) — scout and build \
         dispatch is held; queued work stays queued and nothing is charged an \
         attempt.  {}",
        time::since(hold.since),
        hold.failures,
        time::since(hold.last_seen),
        hold.error
    ))
}

/// The row label for one sealed name. `&'static str` because `fact` takes
/// one, and the set of names is closed.
fn credential_label(name: tasks_client::api::models::SecretName) -> &'static str {
    use tasks_client::api::models::SecretName;
    match name {
        SecretName::AnthropicApiKey => "Anthropic key",
        SecretName::GithubToken => "GitHub token",
    }
}

/// Why new work is waiting, when the reason is that vm-pool has no free slot.
/// Same contract as [`github_hold_line`]: `None` renders no row at all.
///
/// `0 of N` rather than "full" — `0 of 0` is a `VM_POOL_MAX_VMS` that can never
/// dispatch anything, and `0 of 6` is work or a leak holding every slot.
///
/// **Unreachable outranks exhausted, and they share this one row.** Capacity
/// is only askable down a connection that exists, so with no connection the
/// capacity record is stale by construction; printing both would tell a reader
/// that a pool nobody can reach also has no free slots and send them hunting a
/// leaked VM instead of starting a daemon (#991).
fn pool_hold_line(status: &ServerStatus) -> Option<String> {
    if let Some(out) = &status.pool_unreachable {
        return Some(format!(
            "not answering at {} for {} ({} attempt(s), last {} ago) — nothing can be \
             dispatched at all. The server retries the socket on its own, so what \
             needs starting is vm-pool, not this. {}",
            out.socket,
            time::since(out.since),
            out.attempts,
            time::since(out.last_seen),
            out.error
        ));
    }
    let hold = status.pool.as_ref()?;
    Some(format!(
        "0 of {} slots free for {} ({} observation(s), last {} ago) — scout and build \
         dispatch waits for one; queued work stays queued and nothing is charged an \
         attempt.",
        hold.total,
        time::since(hold.since),
        hold.observations,
        time::since(hold.last_seen),
    ))
}

/// Why new work is waiting, when the reason is that the credential broker is
/// not answering. Same contract again: `None` renders no row at all.
///
/// It names the **address**, because that is what the reader checks and
/// because it is deliberately the advertised one and not loopback — loopback
/// answers correctly while the bridge gateway is severed. And it says what
/// dispatching anyway would cost, which is what makes this hold different in
/// kind from the pool's: a clone inside a VM is redeemed at the broker, so
/// work started now does not wait, it dies at the clone and is charged an
/// attempt for it (#1006).
fn broker_hold_line(status: &ServerStatus) -> Option<String> {
    let hold = status.broker.as_ref()?;
    Some(format!(
        "{} not answering for {} ({} probe(s), last {} ago) — scout and build dispatch \
         waits for it; every clone inside a VM is redeemed there, so work started now \
         would die at the clone. {}",
        hold.address,
        time::since(hold.since),
        hold.probes,
        time::since(hold.last_seen),
        hold.error
    ))
}

/// Why new work is waiting, when the reason is that this host's container
/// runtime is not running. Same contract again.
///
/// It quotes what `container system status` said rather than summarising it —
/// a stopped service and a broken install read identically once summarised —
/// and names the discharge, which is one command (#1017).
fn runtime_hold_line(status: &ServerStatus) -> Option<String> {
    let hold = status.runtime.as_ref()?;
    Some(format!(
        "The container runtime has been down for {} ({} probe(s), last {} ago) — nothing \
         here can start a VM, so scout and build dispatch waits. Run `container system \
         start`. {}",
        time::since(hold.since),
        hold.probes,
        time::since(hold.last_seen),
        hold.error
    ))
}

/// How big the orchestrator's warm verification build directory is — the same
/// reading `tasks status` prints.
///
/// **The one row here that is not an exception report.** `github_hold_line`,
/// `update_pending_line` and `pool_hold_line` are `None` while things are fine,
/// because a standing "all clear" is a row a reader learns to skip. This is a
/// quantity that grows silently — it reached 51 GB on a disk with 74 GiB free
/// before a human hunting for space found it (#1010) — so a row that appeared
/// only once it was over its ceiling would reproduce that exactly. `None` here
/// means there is no reading at all: no orchestrator checkout to build in, or
/// no walk yet this boot.
///
/// A reclaim is shown for the rest of the boot, and the wholesale tier says
/// what it cost, because a cold verification is what sends the next batch to a
/// human.
fn verify_dir_line(status: &ServerStatus) -> Option<String> {
    let usage = status.verify_dir.as_ref()?;
    let bound = match usage.budget_bytes {
        Some(budget) if usage.over_budget => {
            format!("OVER its {} ceiling", humanize_bytes(budget))
        }
        Some(budget) => format!("of {}", humanize_bytes(budget)),
        None => "unbounded (ORCHESTRATOR_TARGET_BUDGET_GB=0, report only)".to_string(),
    };
    let mut line = format!(
        "{} {}, measured {} ago",
        humanize_bytes(usage.bytes),
        bound,
        time::since(usage.measured_at)
    );
    if let Some(reclaim) = &usage.last_reclaim {
        let what = match reclaim.tier {
            VerifyDirTier::Incremental => "incremental caches only, no warmth lost",
            VerifyDirTier::Wholesale => "whole directory — the next verification is COLD",
        };
        line.push_str(&format!(
            ".  Reclaimed {} ago: {} -> {}, {what}",
            time::since(reclaim.at),
            humanize_bytes(reclaim.before_bytes),
            humanize_bytes(reclaim.after_bytes),
        ));
    }
    Some(line)
}

/// A size a human can compare to `du -sh` without arithmetic.
///
/// The app's **own** copy of `tasks::verify_dir::humanize_bytes`: this crate
/// depends on `tasks-client`, not on `tasks`, and pulling the server crate in
/// for one formatter would be the wrong trade. What keeps the two sentences in
/// step is the unit test below — decimal units, because the budget is written
/// in GB and the number has to read against the `du -sh` somebody ran.
fn humanize_bytes(bytes: u64) -> String {
    const KB: u64 = 1_000;
    const MB: u64 = 1_000_000;
    const GB: u64 = 1_000_000_000;
    match bytes {
        b if b >= GB => format!("{:.1} GB", b as f64 / GB as f64),
        b if b >= MB => format!("{:.1} MB", b as f64 / MB as f64),
        b if b >= KB => format!("{:.1} kB", b as f64 / KB as f64),
        b => format!("{b} B"),
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
            images: Vec::new(),
            github: None,
            update: None,
            pool: None,
            pool_unreachable: None,
            broker: None,
            runtime: None,
            verify_dir: None,
            orchestrator_lane: None,
        }
    }

    /// Unreachable outranks exhausted and they share the one row: a pool
    /// nobody can reach is not additionally reported as full, which would
    /// send the reader hunting a leaked VM instead of starting a daemon.
    #[test]
    fn an_unreachable_pool_outranks_a_full_one_and_says_which_daemon_to_start() {
        use tasks_client::api::http::{PoolHold, PoolUnreachable};
        let mut status = status();
        status.pool = Some(PoolHold {
            since: Utc::now(),
            last_seen: Utc::now(),
            observations: 9,
            total: 6,
        });
        status.pool_unreachable = Some(PoolUnreachable {
            since: Utc::now(),
            last_seen: Utc::now(),
            attempts: 3,
            socket: "/tmp/vm-pool.sock".into(),
            error: "connection refused".into(),
        });
        let line = pool_hold_line(&status).expect("an unreachable pool is worth a row");
        assert!(line.contains("/tmp/vm-pool.sock"), "{line}");
        assert!(line.contains("not this"), "{line}");
        assert!(
            !line.contains("0 of 6"),
            "the capacity record is stale by construction: {line}"
        );
    }

    /// The row exists only while there is a hold, and when it does it has to
    /// answer the reader's real question: why is nothing being dispatched, and
    /// is work being lost?
    #[test]
    fn a_github_hold_is_named_only_while_it_lasts() {
        use tasks_client::api::http::GitHubHold;

        let mut status = status();
        assert_eq!(github_hold_line(&status), None);

        status.github = Some(GitHubHold {
            since: Utc::now() - chrono::Duration::minutes(12),
            last_seen: Utc::now() - chrono::Duration::seconds(30),
            failures: 12,
            error: "rest: list issues: 503 Service Unavailable: Service Unavailable".into(),
        });
        let line = github_hold_line(&status).expect("a hold is worth a row");
        assert!(line.contains("dispatch is held"), "{line}");
        assert!(
            line.contains("nothing is charged an attempt"),
            "the reader's next question is whether work is being lost: {line}"
        );
        assert!(line.contains("12 failed call"), "{line}");
        assert!(line.contains("503"), "{line}");
    }

    /// The third hold's row, same contract: absent until it binds, and when it
    /// binds it says how full the pool is and what waiting costs.
    #[test]
    fn a_full_pool_is_named_only_while_it_lasts() {
        use tasks_client::api::http::PoolHold;

        let mut status = status();
        assert_eq!(pool_hold_line(&status), None);

        status.pool = Some(PoolHold {
            since: Utc::now() - chrono::Duration::minutes(12),
            last_seen: Utc::now() - chrono::Duration::seconds(30),
            observations: 138,
            total: 6,
        });
        let line = pool_hold_line(&status).expect("a hold is worth a row");
        assert!(line.contains("0 of 6"), "{line}");
        assert!(line.contains("dispatch waits"), "{line}");
        assert!(
            line.contains("nothing is charged an attempt"),
            "the reader's next question is whether work is being lost: {line}"
        );
    }

    /// The one row here that is not an exception report — it appears whenever
    /// there is a reading, because a size that only surfaced once it was over
    /// its ceiling is #1010 (51 GB found by a human hunting for disk).
    ///
    /// And its own `humanize_bytes`: this crate depends on `tasks-client`, not
    /// on `tasks`, so the two copies are kept in step by this test rather than
    /// by the compiler.
    #[test]
    fn the_verification_build_directory_is_a_row_even_when_it_is_fine() {
        use tasks_client::api::http::{VerifyDirReclaim, VerifyDirUsage};

        let mut status = status();
        assert_eq!(verify_dir_line(&status), None, "nothing measured, no row");

        let usage = VerifyDirUsage {
            path: "/state/verify-target".into(),
            bytes: 12_300_000_000,
            files: 213_628,
            measured_at: Utc::now() - chrono::Duration::minutes(5),
            budget_bytes: Some(20_000_000_000),
            over_budget: false,
            last_reclaim: None,
        };
        status.verify_dir = Some(usage.clone());
        let line = verify_dir_line(&status).expect("a reading is always a row");
        assert!(line.contains("12.3 GB of 20.0 GB"), "{line}");

        status.verify_dir = Some(VerifyDirUsage {
            bytes: 51_000_000_000,
            over_budget: true,
            last_reclaim: Some(VerifyDirReclaim {
                at: Utc::now() - chrono::Duration::minutes(3),
                tier: VerifyDirTier::Wholesale,
                before_bytes: 51_000_000_000,
                after_bytes: 0,
            }),
            ..usage
        });
        let line = verify_dir_line(&status).unwrap();
        assert!(line.contains("OVER its 20.0 GB ceiling"), "{line}");
        assert!(line.contains("51.0 GB -> 0 B"), "{line}");
        assert!(
            line.contains("COLD"),
            "a wholesale reclaim names what it cost: {line}"
        );
    }

    /// The same cases `tasks::verify_dir::humanize_bytes` pins, so the two
    /// copies say the same thing. Decimal units: the budget is written in GB
    /// and the number has to read against the `du -sh` somebody ran.
    #[test]
    fn sizes_read_the_way_du_prints_them() {
        assert_eq!(humanize_bytes(0), "0 B");
        assert_eq!(humanize_bytes(999), "999 B");
        assert_eq!(humanize_bytes(1_500), "1.5 kB");
        assert_eq!(humanize_bytes(2_500_000), "2.5 MB");
        assert_eq!(humanize_bytes(51_000_000_000), "51.0 GB");
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

    /// The two readings that must not be confused: an unobserved image is not
    /// a healthy one, and an unstamped one is the state #909 was filed about
    /// rather than a bug in this line.
    #[test]
    fn no_observation_is_not_a_clean_bill_of_health() {
        use tasks_client::api::version::{ImageFreshness, ImageIdentity, ImageRole};

        let mut status = status();
        assert_eq!(images_line(&status), "none observed yet");
        assert!(!images_line(&status).contains("current"));

        status.images = vec![ImageIdentity {
            image: "agent:v1".into(),
            role: ImageRole::Scout,
            version: None,
            commit: None,
            observed_at: Utc::now(),
            run_id: Some("sess_1".into()),
            freshness: ImageFreshness::Unstamped,
        }];
        let line = images_line(&status);
        assert!(line.contains("PREDATES STAMPING"), "{line}");
        assert!(line.contains("unstamped"), "{line}");
        assert!(
            line.contains("make images"),
            "a verdict word alone does not say what to type: {line}"
        );
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
