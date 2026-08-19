//! The machine list: every host running a pool, and what its server says.
//!
//! vm-pool is single-machine today, so the list is usually one entry — this
//! Mac. The shape is a list anyway, because the menu bar app's whole job is
//! "every machine at a glance", and a design that assumed one machine would
//! have that assumption in every render function by the time a second machine
//! exists. Extra machines are named in `TASKS_MENUBAR_MACHINES`
//! (`Name=http://host:4800`, comma-separated); each is just another tasks
//! server reached over its HTTP API, which is the only way this app ever
//! learns anything — the daemon is the product, and this is one more client.
//!
//! Nothing GitHub-owned and nothing pool-owned is persisted or cached here:
//! every fact on screen is one `/status` answer with an age on it, re-read on
//! a timer.

use std::time::Duration;

use chrono::{DateTime, Utc};
use gpui::{App, AppContext as _, Context, Entity, Global};
use tasks_client::api::http::{InFlight, InFlightItem, ServerStatus};
use tasks_client::api::models::Mode;
use tasks_client::Client;

/// How often every machine is probed while nothing is looking — the icon has
/// no state to keep fresh yet, so this exists to make the first open honest
/// rather than to animate anything.
pub const BACKGROUND_POLL: Duration = Duration::from_secs(30);

/// How often the popup re-probes while it is open. Loopback is
/// sub-millisecond; a LAN machine is bounded by the client's call timeout.
pub const OPEN_POLL: Duration = Duration::from_secs(5);

/// Where a machine's server lives, as configured — never probed for.
pub struct MachineSpec {
    pub name: String,
    pub base: String,
}

/// One machine: its spec plus the last probe's answer.
pub struct Machine {
    pub spec: MachineSpec,
    client: Client,
    pub status: Option<ServerStatus>,
    /// The last probe's transport error, when there was no answer at all.
    /// Mutually exclusive with `status` by construction: a probe writes one
    /// and clears the other.
    pub error: Option<String>,
    pub probed_at: Option<DateTime<Utc>>,
    /// Coalesces probes: a machine that is not answering holds a probe for
    /// the client's whole call timeout, and the poll must not stack a second
    /// one behind it.
    probing: bool,
}

impl Machine {
    fn new(spec: MachineSpec) -> Self {
        let client = Client::with_base(spec.base.clone());
        Self {
            spec,
            client,
            status: None,
            error: None,
            probed_at: None,
            probing: false,
        }
    }
}

pub struct Machines {
    pub machines: Vec<Machine>,
}

struct GlobalMachines(Entity<Machines>);

impl Global for GlobalMachines {}

/// Create the global machine list and start the background poll.
pub fn init(cx: &mut App) -> Entity<Machines> {
    let entity = cx.new(|_| Machines::from_env());
    entity.update(cx, |machines, cx| machines.refresh(cx));

    let polled = entity.clone();
    let executor = cx.background_executor().clone();
    cx.spawn(async move |cx| {
        loop {
            executor.timer(BACKGROUND_POLL).await;
            // A strong entity in a global: valid for the app's whole life,
            // and the task dies with the executor at quit.
            polled.update(cx, |machines, cx| machines.refresh(cx));
        }
    })
    .detach();

    cx.set_global(GlobalMachines(entity.clone()));
    entity
}

/// The global instance. Panics if [`init`] has not run.
pub fn global(cx: &App) -> Entity<Machines> {
    cx.global::<GlobalMachines>().0.clone()
}

impl Machines {
    fn from_env() -> Self {
        let mut machines = vec![Machine::new(MachineSpec {
            name: local_machine_name(),
            base: Client::from_env().base_url().to_string(),
        })];
        if let Ok(extra) = std::env::var("TASKS_MENUBAR_MACHINES") {
            machines.extend(
                parse_machine_list(&extra)
                    .into_iter()
                    .map(|(name, base)| Machine::new(MachineSpec { name, base })),
            );
        }
        Self { machines }
    }

    /// Re-probe every machine that is not already mid-probe.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        for index in 0..self.machines.len() {
            self.refresh_one(index, cx);
        }
    }

    fn refresh_one(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(machine) = self.machines.get_mut(index) else {
            return;
        };
        if machine.probing {
            return;
        }
        machine.probing = true;
        let client = machine.client.clone();
        let probe = cx
            .background_executor()
            .spawn(async move { client.status() });
        cx.spawn(async move |this, cx| {
            let result = probe.await;
            this.update(cx, |this: &mut Machines, cx| {
                let Some(machine) = this.machines.get_mut(index) else {
                    return;
                };
                machine.probing = false;
                machine.probed_at = Some(Utc::now());
                match result {
                    Ok(status) => {
                        machine.status = Some(status);
                        machine.error = None;
                    }
                    Err(err) => {
                        machine.status = None;
                        machine.error = Some(err.to_string());
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Cancel every running scout and build on one machine, then re-probe so
    /// the rows disappear as the server concludes them. Cancels are cheap in
    /// the pipeline's terms — no strikes, specs return to `approved`, scout
    /// tasks to `backlog` — which is why this is a menu row and not a dialog.
    pub fn cancel_all(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(machine) = self.machines.get(index) else {
            return;
        };
        let client = machine.client.clone();
        let work = cx.background_executor().spawn(async move {
            client.cancel_all_runs(Some("cancelled from the menu bar".to_string()))
        });
        cx.spawn(async move |this, cx| {
            // The refresh reports the outcome either way; an error here has
            // nowhere better to go than the next probe's answer.
            let _ = work.await;
            this.update(cx, |this: &mut Machines, cx| this.refresh_one(index, cx))
                .ok();
        })
        .detach();
    }
}

/// The name this machine goes by, the way the Mac states it. Falls back down
/// the chain rather than failing: a name is decoration on a section that
/// renders either way.
fn local_machine_name() -> String {
    fn first_line(output: std::process::Output) -> Option<String> {
        let text = String::from_utf8(output.stdout).ok()?;
        let line = text.lines().next()?.trim();
        (!line.is_empty()).then(|| line.to_string())
    }
    #[cfg(target_os = "macos")]
    if let Some(name) = std::process::Command::new("scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .and_then(first_line)
    {
        return name;
    }
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(first_line)
        .unwrap_or_else(|| "this machine".to_string())
}

/// Parse `TASKS_MENUBAR_MACHINES`: comma-separated entries, each
/// `Name=http://host:port` or a bare URL (whose host:port becomes the name).
/// Entries that parse to nothing are skipped rather than failing the list —
/// a typo in one machine must not blank the machine you are standing at.
pub fn parse_machine_list(value: &str) -> Vec<(String, String)> {
    value
        .split(',')
        .filter_map(|entry| {
            let entry = entry.trim();
            if entry.is_empty() {
                return None;
            }
            let (name, base) = match entry.split_once('=') {
                Some((name, base)) => (name.trim().to_string(), base.trim()),
                None => (
                    entry
                        .trim_start_matches("http://")
                        .trim_start_matches("https://")
                        .trim_end_matches('/')
                        .to_string(),
                    entry,
                ),
            };
            if base.is_empty() || name.is_empty() {
                return None;
            }
            Some((name, base.trim_end_matches('/').to_string()))
        })
        .collect()
}

/// "pid 812 · up 4h 12m" — the serving line under a healthy machine.
pub fn serving_line(status: &ServerStatus, now: DateTime<Utc>) -> String {
    format!(
        "pid {} · up {}",
        status.pid,
        uptime((now - status.started_at).num_seconds().max(0))
    )
}

/// One in-flight run as a menu row: what it is, what it is working on, and
/// how long it has been at it.
#[derive(Debug, PartialEq, Eq)]
pub struct RunRow {
    /// "Scout" / "Builder" / "Orchestrator", numbered ("Scout 2") only when
    /// there are several of that kind — a lone "Scout 1" implies a fleet.
    pub kind: String,
    /// The work: the item's `detail` (task title, branch), or its id when the
    /// server sent none.
    pub label: String,
    pub age: String,
}

/// A machine's in-flight work, one row per run, scouts first. Empty means
/// idle, and the caller says so — an empty list renders better as one "idle"
/// line than as nothing.
pub fn run_rows(in_flight: &InFlight, now: DateTime<Utc>) -> Vec<RunRow> {
    fn rows_of(items: &[InFlightItem], kind: &str, now: DateTime<Utc>) -> Vec<RunRow> {
        let numbered = items.len() > 1;
        items
            .iter()
            .enumerate()
            .map(|(i, item)| RunRow {
                kind: if numbered {
                    format!("{kind} {}", i + 1)
                } else {
                    kind.to_string()
                },
                label: item.detail.clone().unwrap_or_else(|| item.id.clone()),
                age: uptime((now - item.since).num_seconds().max(0)),
            })
            .collect()
    }
    let mut rows = rows_of(&in_flight.scouts, "Scout", now);
    rows.extend(rows_of(&in_flight.builds, "Builder", now));
    if let Some(turn) = &in_flight.orchestrator {
        rows.push(RunRow {
            kind: "Orchestrator".to_string(),
            label: turn
                .detail
                .clone()
                .unwrap_or_else(|| "turn owed".to_string()),
            age: uptime((now - turn.since).num_seconds().max(0)),
        });
    }
    rows
}

/// The standing problems worth a line each: a GitHub hold and every pending
/// update reason. Empty for a machine with nothing to say, which is most of
/// them most of the time.
pub fn warning_lines(status: &ServerStatus, now: DateTime<Utc>) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(hold) = &status.github {
        lines.push(format!(
            "GitHub not answering · held {}",
            uptime((now - hold.since).num_seconds().max(0))
        ));
    }
    if let Some(update) = &status.update {
        let verb = if update.enforced {
            "update pending"
        } else {
            // `TASKS_UPDATE_HOLD=off`: reported, not binding.
            "update pending (not enforced)"
        };
        for reason in &update.reasons {
            lines.push(format!("{verb}: {reason}"));
        }
    }
    lines
}

/// "4h 12m" / "3m" / "12s" — coarse on purpose; the menu is a glance.
pub fn uptime(seconds: i64) -> String {
    let (h, m, s) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{s}s")
    }
}

/// What one click on the mode chip asks for. Play pauses; anything quieter
/// plays. Stop is never a click away — it ends intake and the API too, which
/// is not a glance-sized decision.
pub fn toggled_mode(mode: Mode) -> Mode {
    match mode {
        Mode::Play => Mode::Pause,
        Mode::Pause | Mode::Stop => Mode::Play,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tasks_client::api::http::InFlightItem;

    fn item(id: &str) -> InFlightItem {
        InFlightItem {
            id: id.to_string(),
            detail: None,
            since: Utc::now(),
        }
    }

    #[test]
    fn machine_list_parses_named_and_bare_entries() {
        assert_eq!(
            parse_machine_list("studio=http://10.0.0.2:4800, http://10.0.0.3:4800/"),
            vec![
                ("studio".to_string(), "http://10.0.0.2:4800".to_string()),
                (
                    "10.0.0.3:4800".to_string(),
                    "http://10.0.0.3:4800".to_string()
                ),
            ]
        );
    }

    #[test]
    fn machine_list_skips_empty_entries_rather_than_failing() {
        assert_eq!(parse_machine_list(" , =http://x, name= ,"), vec![]);
    }

    #[test]
    fn run_rows_number_only_within_a_crowd() {
        let now = Utc::now();
        let mut in_flight = InFlight::default();
        assert_eq!(run_rows(&in_flight, now), vec![]);

        in_flight.scouts = vec![item("sess_a"), item("sess_b")];
        in_flight.builds = vec![item("build_c")];
        let rows = run_rows(&in_flight, now);
        assert_eq!(
            rows.iter()
                .map(|r| (r.kind.as_str(), r.label.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("Scout 1", "sess_a"),
                ("Scout 2", "sess_b"),
                ("Builder", "build_c"),
            ]
        );
    }

    #[test]
    fn run_rows_prefer_detail_over_id() {
        let mut scout = item("sess_a");
        scout.detail = Some("fix the flaky drain test".to_string());
        let in_flight = InFlight {
            scouts: vec![scout],
            ..Default::default()
        };
        assert_eq!(
            run_rows(&in_flight, Utc::now())[0].label,
            "fix the flaky drain test"
        );
    }

    #[test]
    fn uptime_is_coarse() {
        assert_eq!(uptime(12), "12s");
        assert_eq!(uptime(250), "4m");
        assert_eq!(uptime(15130), "4h 12m");
    }

    #[test]
    fn mode_toggle_never_stops() {
        assert_eq!(toggled_mode(Mode::Play), Mode::Pause);
        assert_eq!(toggled_mode(Mode::Pause), Mode::Play);
        assert_eq!(toggled_mode(Mode::Stop), Mode::Play);
    }
}
