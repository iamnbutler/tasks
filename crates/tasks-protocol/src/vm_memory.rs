//! What the VM's memory did while the agent ran, and what it means.
//!
//! An agent VM that runs out of memory is killed by the kernel, not by
//! anything in this system, and the kill leaves almost no trace: a
//! signal-killed child has no exit code at all, and the OOM killer usually
//! picks the *largest* process — typically a `rustc` or linker job inside the
//! agent's own shell, not the agent. The agent sees one command fail, retries,
//! and can exit 0 having achieved nothing. Neither half is visible from the
//! outside.
//!
//! So both supervisors bracket the agent run with [`sample_memory`] and report
//! the result through [`AgentOutcome`]: the exit code (with signal deaths
//! mapped to `128 + signal`, so SIGKILL is 137), a named signal, and a verdict
//! derived from the cgroup's `oom_kill` counter.
//!
//! Everything here is best-effort. Off cgroup v2 — a macOS host, a unit test —
//! every probe returns `None` and callers say nothing rather than guessing.
//!
//! # `memory.peak` is not evidence of pressure
//!
//! Peak usage counts reclaimable page cache, so a perfectly healthy build sits
//! near its limit by design: a measured `-j1` build of this workspace peaks at
//! 3866 MB of a 4096 MB limit with zero OOM kills, while anonymous memory
//! stays near 1.4 GB. Only the `oom_kill` delta becomes a verdict; peak and
//! anon are reported as information and nothing branches on them.

use std::fs;
use std::path::Path;
use std::process::ExitStatus;

/// Where cgroup v2 is mounted. Inside a container this is the container's own
/// cgroup, which is exactly the accounting we want: the VM's memory, not the
/// host's.
const CGROUP_V2_ROOT: &str = "/sys/fs/cgroup";

const BYTES_PER_MB: u64 = 1024 * 1024;

/// One reading of a cgroup's memory accounting. Every field is independently
/// optional: a kernel that exposes `memory.events` but not `memory.peak` still
/// gives a usable sample.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemorySample {
    /// Cumulative `oom_kill` count from `memory.events`. Monotonic for the
    /// life of the cgroup, which is why it is only meaningful as a delta.
    pub oom_kills: Option<u64>,
    /// High-water mark from `memory.peak`, in MB. Includes page cache — see
    /// the module docs before treating it as pressure.
    pub peak_mb: Option<u64>,
    /// `anon` from `memory.stat`, in MB: the part that cannot be reclaimed
    /// and therefore the part that gets a process killed.
    pub anon_mb: Option<u64>,
    /// The limit from `memory.max`, in MB. `None` means unreadable *or*
    /// unlimited (`max`) — both mean "no number worth printing".
    pub limit_mb: Option<u64>,
}

impl MemorySample {
    fn is_empty(&self) -> bool {
        self.oom_kills.is_none()
            && self.peak_mb.is_none()
            && self.anon_mb.is_none()
            && self.limit_mb.is_none()
    }

    /// One line for a transcript: what the VM had, what it used, whether
    /// anything died.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        match (self.peak_mb, self.limit_mb) {
            (Some(peak), Some(limit)) => parts.push(format!("peak {peak} MB of {limit} MB limit")),
            (Some(peak), None) => parts.push(format!("peak {peak} MB, no limit set")),
            (None, Some(limit)) => parts.push(format!("{limit} MB limit")),
            (None, None) => {}
        }
        if let Some(anon) = self.anon_mb {
            parts.push(format!("{anon} MB anonymous at exit"));
        }
        if let Some(kills) = self.oom_kills {
            parts.push(format!("{kills} OOM kill(s)"));
        }
        if parts.is_empty() {
            return "no cgroup memory accounting available".to_string();
        }
        parts.join(", ")
    }
}

/// Read this process's cgroup v2 memory accounting. `None` when there is no
/// cgroup v2 to read (a macOS host, a test) — never an error, because nothing
/// here is worth failing a run over.
pub fn sample_memory() -> Option<MemorySample> {
    sample_memory_at(Path::new(CGROUP_V2_ROOT))
}

fn sample_memory_at(root: &Path) -> Option<MemorySample> {
    let sample = MemorySample {
        oom_kills: read_keyed(&root.join("memory.events"), "oom_kill"),
        peak_mb: read_bytes(&root.join("memory.peak")).map(to_mb),
        anon_mb: read_keyed(&root.join("memory.stat"), "anon").map(to_mb),
        limit_mb: read_bytes(&root.join("memory.max")).map(to_mb),
    };
    (!sample.is_empty()).then_some(sample)
}

/// A `key value` flat-keyed file (`memory.events`, `memory.stat`).
fn read_keyed(path: &Path, key: &str) -> Option<u64> {
    let text = fs::read_to_string(path).ok()?;
    text.lines()
        .filter_map(|line| line.split_once(' '))
        .find(|(k, _)| *k == key)
        .and_then(|(_, v)| v.trim().parse().ok())
}

/// A single-value file (`memory.peak`, `memory.max`). `max` — cgroup's word
/// for unlimited — reads as `None`.
fn read_bytes(path: &Path) -> Option<u64> {
    let text = fs::read_to_string(path).ok()?;
    text.trim().parse().ok()
}

fn to_mb(bytes: u64) -> u64 {
    bytes / BYTES_PER_MB
}

/// Diagnose a bracketed pair of samples. `Some` only when the kernel actually
/// killed something between the two readings — see the module docs for why
/// peak usage is deliberately not a trigger.
pub fn memory_verdict(before: Option<MemorySample>, after: Option<MemorySample>) -> Option<String> {
    let kills = after?.oom_kills?;
    let baseline = before.and_then(|b| b.oom_kills).unwrap_or(0);
    let delta = kills.saturating_sub(baseline);
    if delta == 0 {
        return None;
    }
    let limit = after
        .and_then(|a| a.limit_mb)
        .map(|mb| format!("{mb} MB"))
        .unwrap_or_else(|| "configured".to_string());
    Some(format!(
        "the kernel OOM-killed {delta} process(es) in this VM during the agent run \
         (memory limit {limit}); the victim is whichever process was largest, usually a \
         compiler or linker job the agent only saw as a failed command"
    ))
}

/// How the agent process ended, with the memory context needed to tell an OOM
/// apart from every other odd failure.
#[derive(Debug, Clone, Default)]
pub struct AgentOutcome {
    /// The agent's exit code, or `128 + signal` when a signal killed it
    /// (SIGKILL = 137). `-1` only when the platform reports neither.
    pub exit_code: i32,
    /// `killed by signal 9 (SIGKILL)`, when the agent died on a signal.
    pub signal: Option<String>,
    /// The OOM verdict from [`memory_verdict`], if anything was killed.
    pub verdict: Option<String>,
    /// The post-run sample (falling back to the pre-run one), for the summary
    /// line every run emits.
    pub memory: Option<MemorySample>,
}

impl AgentOutcome {
    /// Combine an exit status with the samples taken either side of the run.
    pub fn new(
        status: ExitStatus,
        before: Option<MemorySample>,
        after: Option<MemorySample>,
    ) -> Self {
        Self {
            exit_code: exit_code(&status),
            signal: signal_summary(&status),
            verdict: memory_verdict(before, after),
            memory: after.or(before),
        }
    }

    /// The line both supervisors emit after every agent run, OOM or not.
    /// `None` when there was no cgroup to read.
    pub fn memory_summary(&self) -> Option<String> {
        self.memory.map(|m| m.summary())
    }

    /// Suffix to append to a terminal failure reason. Empty when there is
    /// nothing to add — a clean exit on a host with no cgroup accounting says
    /// nothing at all rather than padding the reason with absences.
    pub fn failure_context(&self) -> String {
        let mut out = String::new();
        if let Some(signal) = &self.signal {
            out.push_str(&format!(" — agent {signal}"));
        }
        if let Some(verdict) = &self.verdict {
            out.push_str(&format!(" — {verdict}"));
        }
        out
    }
}

fn exit_code(status: &ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    -1
}

#[cfg(unix)]
fn signal_summary(status: &ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt as _;
    let signal = status.signal()?;
    Some(format!(
        "killed by signal {signal} ({})",
        signal_name(signal)
    ))
}

/// Nothing to report off unix: `ExitStatus` has no signal to read there, and
/// agent VMs are Linux containers regardless.
#[cfg(not(unix))]
fn signal_summary(_status: &ExitStatus) -> Option<String> {
    None
}

#[cfg(unix)]
fn signal_name(signal: i32) -> String {
    match signal {
        1 => "SIGHUP".into(),
        2 => "SIGINT".into(),
        3 => "SIGQUIT".into(),
        4 => "SIGILL".into(),
        6 => "SIGABRT".into(),
        7 => "SIGBUS".into(),
        8 => "SIGFPE".into(),
        9 => "SIGKILL".into(),
        11 => "SIGSEGV".into(),
        13 => "SIGPIPE".into(),
        15 => "SIGTERM".into(),
        other => format!("signal {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fixture cgroup directory. Every file is optional, so the tests
    /// can model a kernel that exposes only some of them.
    fn cgroup(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, contents) in files {
            fs::write(dir.path().join(name), contents).unwrap();
        }
        dir
    }

    const EVENTS_CLEAN: &str = "low 0\nhigh 0\nmax 0\noom 0\noom_kill 0\n";
    const EVENTS_KILLED: &str = "low 0\nhigh 0\nmax 12\noom 3\noom_kill 2\n";

    #[test]
    fn a_healthy_cgroup_samples_every_field() {
        let dir = cgroup(&[
            ("memory.events", EVENTS_CLEAN),
            ("memory.peak", "4054843392\n"),
            ("memory.stat", "anon 1489436672\nfile 2565406720\n"),
            ("memory.max", "4294967296\n"),
        ]);
        let sample = sample_memory_at(dir.path()).expect("a sample");
        assert_eq!(sample.oom_kills, Some(0));
        assert_eq!(sample.peak_mb, Some(3867));
        assert_eq!(sample.anon_mb, Some(1420));
        assert_eq!(sample.limit_mb, Some(4096));
        assert_eq!(
            sample.summary(),
            "peak 3867 MB of 4096 MB limit, 1420 MB anonymous at exit, 0 OOM kill(s)"
        );
    }

    #[test]
    fn an_unlimited_cgroup_reports_no_limit() {
        let dir = cgroup(&[("memory.peak", "1073741824\n"), ("memory.max", "max\n")]);
        let sample = sample_memory_at(dir.path()).expect("a sample");
        assert_eq!(sample.limit_mb, None);
        assert_eq!(sample.summary(), "peak 1024 MB, no limit set");
    }

    /// Off cgroup v2 entirely: no files, no sample, and nothing for a caller
    /// to say.
    #[test]
    fn an_absent_cgroup_samples_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(sample_memory_at(dir.path()), None);
        assert_eq!(memory_verdict(None, None), None);
    }

    #[test]
    fn kills_during_the_run_are_the_delta_not_the_total() {
        let before = cgroup(&[("memory.events", EVENTS_KILLED)]);
        let after = cgroup(&[("memory.events", EVENTS_KILLED)]);
        let before = sample_memory_at(before.path());
        let after = sample_memory_at(after.path());
        // Same total on both sides: the kills predate this run.
        assert_eq!(memory_verdict(before, after), None);
    }

    #[test]
    fn a_kill_during_the_run_is_a_verdict_naming_the_limit() {
        let before = cgroup(&[("memory.events", EVENTS_CLEAN)]);
        let after = cgroup(&[
            ("memory.events", EVENTS_KILLED),
            ("memory.max", "4294967296\n"),
        ]);
        let verdict = memory_verdict(
            sample_memory_at(before.path()),
            sample_memory_at(after.path()),
        )
        .expect("a verdict");
        assert!(verdict.contains("OOM-killed 2 process(es)"), "{verdict}");
        assert!(verdict.contains("4096 MB"), "{verdict}");
    }

    /// Peak within a hair of the limit, zero kills: silence. A draft of this
    /// module warned here and would have fired on every healthy build.
    #[test]
    fn a_peak_near_the_limit_is_not_a_verdict() {
        let before = cgroup(&[("memory.events", EVENTS_CLEAN), ("memory.peak", "0\n")]);
        let after = cgroup(&[
            ("memory.events", EVENTS_CLEAN),
            ("memory.peak", "4160749568\n"),
            ("memory.max", "4294967296\n"),
        ]);
        assert_eq!(
            memory_verdict(
                sample_memory_at(before.path()),
                sample_memory_at(after.path())
            ),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_signal_death_is_128_plus_the_signal_and_says_which() {
        use std::os::unix::process::ExitStatusExt as _;

        let outcome = AgentOutcome::new(ExitStatus::from_raw(9), None, None);
        assert_eq!(outcome.exit_code, 137);
        assert_eq!(
            outcome.failure_context(),
            " — agent killed by signal 9 (SIGKILL)"
        );
    }

    #[test]
    fn a_clean_exit_with_no_cgroup_says_nothing() {
        let status = std::process::Command::new("true").status().unwrap();
        let outcome = AgentOutcome::new(status, None, None);
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.failure_context(), "");
        assert_eq!(outcome.memory_summary(), None);
    }
}
