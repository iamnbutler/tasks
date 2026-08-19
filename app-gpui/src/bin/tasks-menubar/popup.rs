//! The dropdown: a gpui PopUp panel under the status item.
//!
//! Its structure is menu-shaped, top to bottom: a **SERVER** section (the
//! local daemon's lifecycle — Start/Stop by state, Restart, Kill All Runs),
//! then one section per **machine** running the pool (name, mode chip, one
//! row per in-flight scout/builder), then Open Tasks and Quit. The server ops
//! come from the same `server.rs` the app's Server menu drives — see
//! `main.rs` for the sharing arrangement — so both front ends resolve the
//! same binary and speak the same verdicts.
//!
//! gpui's `WindowKind::PopUp` is a non-activating panel at popup window level
//! that can still become key — so showing it steals no focus from whatever
//! the user was doing, and clicking anywhere else makes it resign key, which
//! gpui reports through the window activation observer. Dismissal is
//! therefore pure gpui: deactivate → remove the window. The one seam that
//! needs care is the status item itself — clicking it while the popup is open
//! can fire *both* the resign-key dismissal and the toggle action, and
//! without the [`REOPEN_DEBOUNCE`] the toggle would find no popup and
//! cheerfully reopen the one it was asked to close.
//!
//! The window is created fresh on every open and sized to its content —
//! there is no way to ask a window to hug its measured content in gpui. So
//! render and [`estimated_height`] are fed by the same *plans*
//! ([`server_section`], [`machine_lines`]): one function decides what a
//! section contains, the other two only draw or count it, and a row added in
//! one place is priced in both. Render also re-checks the height every frame
//! and resizes, which is what keeps the panel hugging rows that appear while
//! it is open.

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use gpui::prelude::*;
use gpui::{
    actions, div, point, px, size, App, Bounds, Context, Entity, FocusHandle, Global, Hsla,
    KeyBinding, SharedString, Window, WindowBackgroundAppearance, WindowBounds, WindowHandle,
    WindowKind, WindowOptions,
};
use gpuikit::theme::{ActiveTheme, Themeable};
use tasks_client::api::models::Mode;

use crate::machines::{self, Machine, Machines, RunRow};
use crate::server::{Op, ServerControl};

/// The popup renders in the workspace app's face; a menu is still Tasks.
const FONT: &str = "Menlo";

const WIDTH: f32 = 320.0;
/// Root padding, top and bottom each.
const PAD: f32 = 6.0;
/// One section: vertical padding around its rows.
const SECTION_PAD: f32 = 6.0;
/// A section's label row: "SERVER", or a machine's dot + name + mode chip.
const HEADER_HEIGHT: f32 = 22.0;
/// One informational line (serving, run row, warning).
const LINE_HEIGHT: f32 = 17.0;
/// One clickable row (server ops, footer).
const ACTION_HEIGHT: f32 = 24.0;
/// A separator line plus its margins.
const SEPARATOR_HEIGHT: f32 = 9.0;

/// How long after a dismissal the toggle treats "open" as "that click was the
/// close". Covers the resign-key → action ordering of a status item click.
const REOPEN_DEBOUNCE: Duration = Duration::from_millis(400);

const KEY_CONTEXT: &str = "MenubarPopup";

actions!(menubar, [Dismiss]);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", Dismiss, Some(KEY_CONTEXT))]);
}

/// Where the status item sits, in gpui's global coordinates (top-left origin,
/// logical pixels — the same units as AppKit points).
#[derive(Debug, Clone, Copy)]
pub struct Anchor {
    /// Left edge of the status item's button.
    pub x: f64,
    /// Bottom edge of the menu bar under it, where the popup's top goes.
    pub bottom: f64,
}

/// The open popup, if any, plus when the last one closed — the debounce's
/// only input.
#[derive(Default)]
struct PopupState {
    handle: Option<WindowHandle<Popup>>,
    closed_at: Option<Instant>,
}

impl Global for PopupState {}

/// Open the popup under `anchor`, or close the one that is open.
pub fn toggle(cx: &mut App, anchor: Anchor) {
    let state = cx.default_global::<PopupState>();
    if let Some(handle) = state.handle.take() {
        state.closed_at = Some(Instant::now());
        handle
            .update(cx, |_, window, _| window.remove_window())
            .ok();
        return;
    }
    let recently_closed = state
        .closed_at
        .is_some_and(|at| at.elapsed() < REOPEN_DEBOUNCE);
    if recently_closed {
        // The resign-key half of this same click already closed it.
        return;
    }
    open(cx, anchor);
}

fn open(cx: &mut App, anchor: Anchor) {
    let machines = machines::global(cx);
    let server = ServerControl::global(cx);
    // The menu is a claim about right now, so every open starts a probe; the
    // stale answer renders in the meantime with its age implied by change.
    machines.update(cx, |machines, cx| machines.refresh(cx));
    server.update(cx, |server, cx| server.refresh(cx));

    let height = estimated_height(server.read(cx), machines.read(cx), Utc::now());
    let mut x = anchor.x as f32;
    if let Some(display) = cx.primary_display() {
        let right = f32::from(display.bounds().right()) - 8.0;
        x = x.min(right - WIDTH);
    }
    let bounds = Bounds {
        origin: point(px(x), px(anchor.bottom as f32 + 4.0)),
        size: size(px(WIDTH), px(height)),
    };

    let handle = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: None,
            focus: true,
            show: true,
            kind: WindowKind::PopUp,
            is_movable: false,
            is_resizable: false,
            is_minimizable: false,
            window_background: WindowBackgroundAppearance::Transparent,
            ..Default::default()
        },
        |window, cx| cx.new(|cx| Popup::new(machines, server, true, window, cx)),
    );
    match handle {
        Ok(handle) => cx.default_global::<PopupState>().handle = Some(handle),
        Err(error) => eprintln!("failed to open the menu bar popup: {error}"),
    }
}

/// The non-mac entry point: the same view in a plain window, so the binary
/// runs where it is developed (a Linux agent VM) even though the status bar
/// it exists for does not. No auto-dismiss — a dev window that died on focus
/// loss would be unusable.
#[allow(dead_code)]
pub fn open_detached(cx: &mut App) {
    let machines = machines::global(cx);
    let server = ServerControl::global(cx);
    let bounds = Bounds::centered(None, size(px(WIDTH), px(400.)), cx);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        },
        |window, cx| cx.new(|cx| Popup::new(machines, server, false, window, cx)),
    )
    .ok();
    cx.activate(true);
}

pub struct Popup {
    machines: Entity<Machines>,
    server: Entity<ServerControl>,
    focus: FocusHandle,
    /// Whether losing key status closes the window — true under the status
    /// item, false for the detached dev window.
    auto_dismiss: bool,
}

impl Popup {
    fn new(
        machines: Entity<Machines>,
        server: Entity<ServerControl>,
        auto_dismiss: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&machines, |_, _, cx| cx.notify()).detach();
        cx.observe(&server, |_, _, cx| cx.notify()).detach();

        if auto_dismiss {
            cx.observe_window_activation(window, |this, window, cx| {
                if !window.is_window_active() {
                    this.dismiss(window, cx);
                }
            })
            .detach();
        }

        // Re-probe while open, on the popup's faster clock; ends with this
        // view, which ends with the window.
        let polled = machines.clone();
        let probed = server.clone();
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            loop {
                executor.timer(machines::OPEN_POLL).await;
                // The weak handle is the exit condition: the view dies with
                // the window, the globals do not.
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
                polled.update(cx, |machines, cx| machines.refresh(cx));
                probed.update(cx, |server, cx| server.refresh(cx));
            }
        })
        .detach();

        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        Self {
            machines,
            server,
            focus,
            auto_dismiss,
        }
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.auto_dismiss {
            return;
        }
        let state = cx.default_global::<PopupState>();
        state.handle = None;
        state.closed_at = Some(Instant::now());
        window.remove_window();
    }

    fn run_server_action(&mut self, action: ServerAction, cx: &mut Context<Self>) {
        match action {
            // Start and restart are one op: `tasks reload` with nothing
            // serving *is* a start — server.rs's own rule.
            ServerAction::Start | ServerAction::Restart => {
                self.server.update(cx, |server, cx| {
                    server.start(Op::Restart, cx);
                });
            }
            // `request`, not `start`: with work in flight this parks the
            // question, and the section renders it as Stop Anyway / Keep
            // Running on the next frame.
            ServerAction::Stop => {
                self.server.update(cx, |server, cx| {
                    server.request(Op::Stop, cx);
                });
            }
            ServerAction::StopAnyway => {
                self.server.update(cx, |server, cx| {
                    server.start(Op::Stop, cx);
                });
            }
            ServerAction::KeepRunning => {
                self.server
                    .update(cx, |server, cx| server.cancel_pending(cx));
            }
            // The SERVER section speaks for the local machine, index 0 by
            // construction (see `Machines::from_env`).
            ServerAction::KillAll => {
                self.machines
                    .update(cx, |machines, cx| machines.cancel_all(0, cx));
            }
        }
    }
}

impl Render for Popup {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let now = Utc::now();

        // Rows arriving mid-open add height; keep the panel hugging them.
        // Only under the status item — the detached dev window is the user's
        // to size.
        if self.auto_dismiss {
            let wanted = estimated_height(self.server.read(cx), self.machines.read(cx), now);
            if (f32::from(window.bounds().size.height) - wanted).abs() > 0.5 {
                window.resize(size(px(WIDTH), px(wanted)));
            }
        }

        let machine_count = self.machines.read(cx).machines.len();
        let mut body = Vec::new();
        body.push(self.render_server(now, cx).into_any_element());
        for index in 0..machine_count {
            body.push(separator(theme.border_subtle()).into_any_element());
            body.push(self.render_machine(index, now, cx).into_any_element());
        }
        body.push(separator(theme.border_subtle()).into_any_element());
        body.push(self.render_footer(cx).into_any_element());

        div()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Dismiss, window, cx| this.dismiss(window, cx)))
            .size_full()
            .rounded(px(10.))
            .bg(theme.bg())
            .border_1()
            .border_color(theme.border())
            .font_family(FONT)
            .text_size(px(12.))
            .text_color(theme.fg())
            .overflow_hidden()
            .flex()
            .flex_col()
            .py(px(PAD))
            .children(body)
    }
}

impl Popup {
    fn render_server(&self, now: DateTime<Utc>, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let section = server_section(self.server.read(cx), now);

        let label = div()
            .h(px(HEADER_HEIGHT))
            .flex()
            .items_center()
            .text_size(px(10.))
            .text_color(theme.fg_muted())
            .child("SERVER");

        let info = section
            .info
            .into_iter()
            .enumerate()
            .map(|(i, (text, tone))| {
                div()
                    .id(SharedString::from(format!("server-info-{i}")))
                    .h(px(LINE_HEIGHT))
                    .text_size(px(11.))
                    .text_color(tone.color(theme.as_ref()))
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(text)
            });

        let actions = section.actions.into_iter().map(|action| {
            let color = match action {
                ServerAction::KillAll | ServerAction::StopAnyway => theme.danger(),
                _ => theme.fg(),
            };
            let hover_bg = theme.surface_secondary();
            div()
                .id(SharedString::from(format!("server-{}", action.id())))
                .h(px(ACTION_HEIGHT))
                .px(px(8.))
                .mx(px(-8.))
                .rounded(px(5.))
                .flex()
                .items_center()
                .text_color(color)
                .hover(|style| style.bg(hover_bg))
                .child(action.label())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.run_server_action(action, cx);
                }))
        });

        div()
            .flex()
            .flex_col()
            .px(px(12.))
            .py(px(SECTION_PAD))
            .child(label)
            .children(info)
            .children(actions)
    }

    fn render_machine(
        &self,
        index: usize,
        now: DateTime<Utc>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let machines = self.machines.read(cx);
        let machine = &machines.machines[index];

        let name = machine.spec.name.clone();
        let dot = machine_dot(machine, theme.as_ref());
        let mode = machine.status.as_ref().map(|status| status.mode);
        let lines = machine_lines(machine, now);

        let header = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .h(px(HEADER_HEIGHT))
            .child(div().size(px(7.)).rounded_full().flex_none().bg(dot))
            .child(
                div()
                    .flex_grow(1.)
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(name),
            )
            .children(mode.map(|mode| self.render_mode_chip(index, mode, cx)));

        div()
            .flex()
            .flex_col()
            .px(px(12.))
            .py(px(SECTION_PAD))
            .child(header)
            .children(lines.into_iter().enumerate().map(|(i, line)| {
                let row = div()
                    .id(SharedString::from(format!("line-{index}-{i}")))
                    .h(px(LINE_HEIGHT))
                    .pl(px(13.))
                    .text_size(px(11.))
                    .flex()
                    .items_center()
                    .gap(px(6.));
                match line {
                    MachineLine::Text(text, tone) => {
                        row.text_color(tone.color(theme.as_ref())).child(
                            div()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(text),
                        )
                    }
                    MachineLine::Run(run) => row
                        .child(div().flex_none().text_color(theme.fg()).child(run.kind))
                        .child(
                            div()
                                .flex_grow(1.)
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .text_color(theme.fg_muted())
                                .child(run.label),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_color(theme.fg_muted())
                                .child(run.age),
                        ),
                }
            }))
    }

    /// The mode as a chip, and the chip as the one control on a machine:
    /// click toggles play ↔ pause (never stop — see
    /// [`machines::toggled_mode`]).
    fn render_mode_chip(
        &self,
        index: usize,
        mode: Mode,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let color = match mode {
            Mode::Play => theme.success(),
            Mode::Pause => theme.warning(),
            Mode::Stop => theme.danger(),
        };
        let machines = self.machines.clone();
        div()
            .id(SharedString::from(format!("mode-{index}")))
            .flex_none()
            .px(px(6.))
            .py(px(1.))
            .rounded(px(4.))
            .border_1()
            .border_color(color)
            .text_size(px(10.))
            .text_color(color)
            .hover(|style| style.bg(theme.surface_secondary()))
            .child(mode.as_str().to_uppercase())
            .on_click(move |_, _, cx| {
                machines.update(cx, |m, cx| {
                    m.set_mode(index, machines::toggled_mode(mode), cx)
                });
            })
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut rows = Vec::new();
        #[cfg(target_os = "macos")]
        rows.push(
            menu_row("open-app", "Open Tasks", cx, |_, _| {
                // Fire-and-forget: if the app is not installed, `open` says
                // so on its own stderr and the menu has nothing to add.
                let _ = std::process::Command::new("open")
                    .args(["-a", "Tasks"])
                    .spawn();
            })
            .into_any_element(),
        );
        rows.push(menu_row("quit", "Quit", cx, |_, cx| cx.quit()).into_any_element());

        div().flex().flex_col().px(px(4.)).children(rows)
    }
}

/// One clickable row, menu-item shaped.
fn menu_row(
    id: &'static str,
    label: &'static str,
    cx: &mut Context<Popup>,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let theme = cx.theme().clone();
    div()
        .id(id)
        .h(px(ACTION_HEIGHT))
        .px(px(8.))
        .rounded(px(5.))
        .flex()
        .items_center()
        .text_size(px(12.))
        .hover(|style| style.bg(theme.surface_secondary()))
        .child(label)
        .on_click(move |_, window, cx| on_click(window, cx))
}

fn separator(color: Hsla) -> impl IntoElement {
    div().my(px(4.)).h(px(1.)).mx(px(8.)).bg(color)
}

/// The dot next to a machine's name: answered green, failed red, never
/// probed muted. Holds don't dim it — they get their own lines.
fn machine_dot(machine: &Machine, theme: &impl Themeable) -> Hsla {
    if machine.status.is_some() {
        theme.success()
    } else if machine.error.is_some() {
        theme.danger()
    } else {
        theme.fg_disabled()
    }
}

/// How a non-run line reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Muted,
    Warn,
    Danger,
}

impl Tone {
    fn color(self, theme: &impl Themeable) -> Hsla {
        match self {
            Tone::Muted => theme.fg_muted(),
            Tone::Warn => theme.warning(),
            Tone::Danger => theme.danger(),
        }
    }
}

/// One line of a machine section's body.
pub enum MachineLine {
    Text(String, Tone),
    Run(RunRow),
}

/// What one machine section's body contains. Render draws exactly this,
/// [`estimated_height`] counts exactly this.
pub fn machine_lines(machine: &Machine, now: DateTime<Utc>) -> Vec<MachineLine> {
    match (&machine.status, &machine.error) {
        (Some(status), _) => {
            let mut lines = vec![MachineLine::Text(
                machines::serving_line(status, now),
                Tone::Muted,
            )];
            let runs = machines::run_rows(&status.in_flight, now);
            if runs.is_empty() {
                lines.push(MachineLine::Text("idle".to_string(), Tone::Muted));
            } else {
                lines.extend(runs.into_iter().map(MachineLine::Run));
            }
            lines.extend(
                machines::warning_lines(status, now)
                    .into_iter()
                    .map(|warning| MachineLine::Text(warning, Tone::Warn)),
            );
            lines
        }
        (None, Some(error)) => vec![MachineLine::Text(
            format!("not serving — {error}"),
            Tone::Danger,
        )],
        (None, None) => vec![MachineLine::Text("probing…".to_string(), Tone::Muted)],
    }
}

/// The rows the SERVER section offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerAction {
    Start,
    Stop,
    Restart,
    KillAll,
    StopAnyway,
    KeepRunning,
}

impl ServerAction {
    pub fn label(self) -> &'static str {
        match self {
            ServerAction::Start => "Start Server",
            ServerAction::Stop => "Stop Server",
            ServerAction::Restart => "Restart Server",
            ServerAction::KillAll => "Kill All Runs",
            ServerAction::StopAnyway => "Stop Anyway",
            ServerAction::KeepRunning => "Keep Running",
        }
    }

    fn id(self) -> &'static str {
        match self {
            ServerAction::Start => "start",
            ServerAction::Stop => "stop",
            ServerAction::Restart => "restart",
            ServerAction::KillAll => "kill-all",
            ServerAction::StopAnyway => "stop-anyway",
            ServerAction::KeepRunning => "keep-running",
        }
    }
}

/// Which action rows the SERVER section offers, from where the server is.
///
/// While an op runs there are none — the running line stands in for them,
/// and a second op would be refused anyway ([`ServerControl::start`]). A
/// parked stop question replaces everything with its two answers. Otherwise
/// the state picks: serving offers Stop/Restart (plus Kill All when work is
/// in flight to kill), stopped offers Start.
pub fn server_actions(
    serving: bool,
    busy: bool,
    pending_stop: bool,
    destructible: bool,
) -> Vec<ServerAction> {
    if busy {
        return Vec::new();
    }
    if pending_stop {
        return vec![ServerAction::StopAnyway, ServerAction::KeepRunning];
    }
    if serving {
        let mut actions = vec![ServerAction::Stop, ServerAction::Restart];
        if destructible {
            actions.push(ServerAction::KillAll);
        }
        actions
    } else {
        vec![ServerAction::Start]
    }
}

/// The SERVER section's whole body: info lines, then action rows. One plan
/// that render draws and [`estimated_height`] counts.
pub struct ServerSection {
    pub info: Vec<(String, Tone)>,
    pub actions: Vec<ServerAction>,
}

pub fn server_section(control: &ServerControl, now: DateTime<Utc>) -> ServerSection {
    let serving = control.status.is_some();
    let busy = control.busy();
    let pending = control.pending.is_some();
    let destructible = control.destructible().is_some();

    let mut info = Vec::new();
    if let Some(run) = &control.run {
        if run.is_running() {
            info.push((
                format!(
                    "{}… {}",
                    run.op.label(),
                    machines::uptime((now - run.started_at).num_seconds().max(0))
                ),
                Tone::Muted,
            ));
        } else if let Some(outcome) = run.outcome {
            // A failure or refusal stands until the next op; success needs no
            // line, because the section's own state (serving, actions) is the
            // report.
            if !outcome.is_success() {
                info.push((outcome.headline(run.op), Tone::Warn));
            }
        }
    }
    if pending {
        let in_flight = control
            .destructible()
            .map(|work| work.scouts.len() + work.builds.len())
            .unwrap_or(0);
        info.push((format!("{in_flight} running — stop anyway?"), Tone::Warn));
    }

    ServerSection {
        info,
        actions: server_actions(serving, busy, pending, destructible),
    }
}

/// The height the render above will lay out to, computed from the same plans
/// and constants it draws with. gpui cannot size a window to content, so the
/// popup is born at this height and resized to it as the plans change.
pub fn estimated_height(server: &ServerControl, machines: &Machines, now: DateTime<Utc>) -> f32 {
    let mut height = PAD * 2.0;

    let section = server_section(server, now);
    height += SECTION_PAD * 2.0
        + HEADER_HEIGHT
        + LINE_HEIGHT * section.info.len() as f32
        + ACTION_HEIGHT * section.actions.len() as f32;

    for machine in &machines.machines {
        height += SEPARATOR_HEIGHT
            + SECTION_PAD * 2.0
            + HEADER_HEIGHT
            + LINE_HEIGHT * machine_lines(machine, now).len() as f32;
    }

    height += SEPARATOR_HEIGHT;
    let footer_rows = if cfg!(target_os = "macos") { 2.0 } else { 1.0 };
    height += ACTION_HEIGHT * footer_rows;
    height
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stopped_server_offers_start_and_nothing_else() {
        assert_eq!(
            server_actions(false, false, false, false),
            vec![ServerAction::Start]
        );
    }

    #[test]
    fn a_serving_server_offers_stop_and_restart_and_kill_all_only_with_work() {
        assert_eq!(
            server_actions(true, false, false, false),
            vec![ServerAction::Stop, ServerAction::Restart]
        );
        assert_eq!(
            server_actions(true, false, false, true),
            vec![
                ServerAction::Stop,
                ServerAction::Restart,
                ServerAction::KillAll
            ]
        );
    }

    #[test]
    fn a_running_op_suspends_the_actions() {
        assert_eq!(server_actions(true, true, false, true), vec![]);
    }

    #[test]
    fn a_parked_stop_question_replaces_the_actions_with_its_answers() {
        assert_eq!(
            server_actions(true, false, true, true),
            vec![ServerAction::StopAnyway, ServerAction::KeepRunning]
        );
    }
}
