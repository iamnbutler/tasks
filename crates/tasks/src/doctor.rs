//! `tasks doctor` — whether this machine can actually run a scout.
//!
//! Every precondition for a scout, asked at once and printed as a checklist in
//! the order the preconditions bite: the environment files and the data dir,
//! whether the configuration parses at all, the container CLI and its system
//! services, the toolchain `make images` needs, vm-pool's socket and protocol
//! and its two ledgers, the server, the images, credential custody, the
//! credential broker the VMs redeem against, GitHub's answer to this token,
//! whether any project is tracked, and the orchestrator's surroundings.
//!
//! # What it is allowed to touch
//!
//! It **reports and never fixes**: every failing check carries the command
//! that changes it, and that is enforced by [`Check::fail`]'s and
//! [`Check::warn`]'s signatures rather than by review — a required parameter
//! cannot be forgotten, because it does not compile. Resist a `--fix` flag: a
//! diagnostic that changes state cannot be run when you are unsure, and the
//! one fix it would most want to perform (`make images`) cannot be reached
//! from inside this pipeline at all.
//!
//! It writes nothing — not to GitHub, not to the store, not to a VM. In
//! particular **it never opens the store**, because `Store::open` runs
//! migrations and a diagnostic that moved the schema would be the one thing
//! worse than no diagnostic; this is the same rule `reload` follows, and it is
//! why mode, projects and the observed image identities come from the running
//! server's HTTP API rather than from the database. A host with no server
//! reports "not serving" instead of reaching past it.
//!
//! There is exactly one deliberate exception, called out here rather than
//! buried: the **data-dir write probe**. Writability is only answerable by
//! writing — mode bits lie under ACLs, a read-only mount and a full disk — so
//! it creates one uniquely-named file under the data dir and removes it. It
//! touches no path this system reads, and a test asserts the directory is as
//! it found it.
//!
//! And it never prints a credential, only which source answered. That is
//! structural rather than careful: the type it prints
//! ([`crate::secrets::CredentialSource`]) has no value in it.
//!
//! # The four levels
//!
//! [`Level`] is a set of meanings, not a severity scale:
//!
//! - **Fail** — a required capability is missing or broken. A scout
//!   dispatched now would not start, or would start and die.
//! - **Warn** — everything required is present, but something is degraded (a
//!   pool with no slack, a stale image) or is deliberately set not to run
//!   (mode `pause`, no project tracked). A warning is not a defect in the
//!   machine; it is a fact about the choices made on it.
//! - **Skip** — the check could not be *made*, and says why. Never a pass,
//!   and never an exit code: every skip has a failure above it that caused
//!   it, and that failure is what fails the run. A skip that failed too would
//!   report one broken thing as two.
//! - **Ok** — asked and answered.
//!
//! Exit is `0` clean, `1` when a required check failed — or, under
//! `--strict`, when anything warned — and `2` on a usage error, so
//! `tasks doctor && …` can be the first line of a setup script.
//!
//! # Nothing short-circuits
//!
//! A failure that invalidates later checks emits them as [`Level::Skip`] with
//! the reason rather than omitting them, so the output has the same shape
//! every time and a reader can tell "not asked" from "not present". The
//! ordering is load-bearing ergonomics and not decoration — a missing
//! container CLI explains the vm-pool failure below it, which explains the
//! dispatch failure below that. Do not sort by severity.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long any one external probe (a subprocess, a socket, an HTTP call) may
/// take. A doctor that hangs is worse than one that reports no answer.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// The opt-in cold image read boots a container, which is seconds rather than
/// milliseconds.
const IMAGE_PROBE_TIMEOUT: Duration = Duration::from_secs(90);

/// What one check concluded. See the module docs: these are meanings, not a
/// scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
    Skip,
}

impl Level {
    /// Whether this level is one a reader has to act on. `Skip` is not: it
    /// means the question could not be asked, and something above it says why.
    pub fn is_bad(self) -> bool {
        matches!(self, Level::Warn | Level::Fail)
    }

    fn marker(self) -> &'static str {
        match self {
            Level::Ok => "ok  ",
            Level::Warn => "warn",
            Level::Fail => "FAIL",
            Level::Skip => "skip",
        }
    }
}

/// One question, its answer, and — when the answer is bad — the command that
/// changes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub level: Level,
    pub detail: String,
    /// The fix. Present on every [`Level::Fail`] and on every [`Level::Warn`]
    /// except the two built by [`Check::note`], which are the warnings with
    /// genuinely no command behind them.
    pub fix: Option<String>,
}

impl Check {
    pub fn ok(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            level: Level::Ok,
            detail: detail.into(),
            fix: None,
        }
    }

    /// A failure **and** the command that changes it.
    ///
    /// The fix is taken by value rather than as an `Option` deliberately.
    /// Every earlier version of "name the fix beside the complaint" in this
    /// tree — `make check-toolchain`, `make images-check`, `ImageFreshness`,
    /// the update-hold reasons — does it by convention, and a convention is
    /// exactly what the next check added here would quietly skip.
    pub fn fail(
        name: impl Into<String>,
        detail: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            level: Level::Fail,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }

    /// A degradation, and the command that clears it. Same signature rule as
    /// [`Check::fail`], for the same reason.
    pub fn warn(
        name: impl Into<String>,
        detail: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            level: Level::Warn,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }

    /// A warning with genuinely nothing to run — "nothing has been observed
    /// yet", "this token type does not enumerate its permissions".
    ///
    /// The named escape hatch from the rule above, and it is named precisely
    /// so that "there is nothing to run" cannot be mistaken for "somebody
    /// forgot to write the fix down". Reach for it only when you can say why
    /// no command exists.
    pub fn note(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            level: Level::Warn,
            detail: detail.into(),
            fix: None,
        }
    }

    /// A question that could not be asked, and why. Never a pass, and never
    /// an exit code.
    pub fn skip(name: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            level: Level::Skip,
            detail: reason.into(),
            fix: None,
        }
    }
}

/// A group of checks under one heading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub title: String,
    pub checks: Vec<Check>,
}

impl Section {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            checks: Vec::new(),
        }
    }

    pub fn push(&mut self, check: Check) {
        self.checks.push(check);
    }
}

/// The whole checklist. Plain data over plain data: [`Report::exit_code`] and
/// the [`fmt::Display`] impl are pure, so every path through them is unit
/// testable without a host — which is most of what makes a diagnostic
/// trustworthy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub sections: Vec<Section>,
}

impl Report {
    pub fn push(&mut self, section: Section) {
        self.sections.push(section);
    }

    pub fn checks(&self) -> impl Iterator<Item = &Check> {
        self.sections.iter().flat_map(|s| s.checks.iter())
    }

    fn count(&self, level: Level) -> usize {
        self.checks().filter(|c| c.level == level).count()
    }

    /// `0` clean, `1` when something required failed — or, under `strict`,
    /// when anything warned.
    ///
    /// A [`Level::Skip`] never sets it. Every skip has a failure above it that
    /// caused it, and that failure is what fails the run; a skip that failed
    /// too would report one broken thing as two, and `--strict` on a machine
    /// with no container CLI would be unreadable.
    pub fn exit_code(&self, strict: bool) -> i32 {
        if self.count(Level::Fail) > 0 {
            return 1;
        }
        if strict && self.count(Level::Warn) > 0 {
            return 1;
        }
        0
    }

    /// The one-line verdict at the bottom, which is what a human actually
    /// reads first.
    fn summary(&self) -> String {
        let (fail, warn, skip) = (
            self.count(Level::Fail),
            self.count(Level::Warn),
            self.count(Level::Skip),
        );
        let mut out = match fail {
            0 => "no failures".to_string(),
            n => format!("{n} failure(s)"),
        };
        if warn > 0 {
            out.push_str(&format!(", {warn} warning(s)"));
        }
        if skip > 0 {
            out.push_str(&format!(", {skip} not asked"));
        }
        out
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for section in &self.sections {
            writeln!(f, "{}", section.title)?;
            for check in &section.checks {
                writeln!(
                    f,
                    "  {}  {:<22}  {}",
                    check.level.marker(),
                    check.name,
                    check.detail
                )?;
                if let Some(fix) = &check.fix {
                    writeln!(f, "        {:<22}  -> {fix}", "")?;
                }
            }
            writeln!(f)?;
        }
        write!(f, "{}", self.summary())
    }
}

// ---------------------------------------------------------------------------
// Probes
// ---------------------------------------------------------------------------

/// What asking an external tool produced.
///
/// Four outcomes rather than a `Result`, because the three failures want three
/// different levels: a tool that is not installed is a `Fail` naming the
/// install, one that ran and refused is a `Fail` naming its own words, and one
/// that hung is a `Skip` — we did not learn the answer, and saying we did
/// would be the one thing a diagnostic must not do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// It ran. `ok` is the exit status; `text` is the first useful line.
    Ran { ok: bool, text: String },
    /// There is no such executable on `PATH`.
    Missing,
    /// It did not finish inside [`PROBE_TIMEOUT`].
    TimedOut,
    /// It could not be spawned for some other reason.
    Failed(String),
}

impl Probe {
    fn succeeded(&self) -> bool {
        matches!(self, Probe::Ran { ok: true, .. })
    }

    /// The first useful line, or the failure rendered — prose for a reader.
    pub fn describe(&self) -> &str {
        self.text()
    }

    fn text(&self) -> &str {
        match self {
            Probe::Ran { text, .. } => text,
            Probe::Missing => "not installed",
            Probe::TimedOut => "timed out",
            Probe::Failed(e) => e,
        }
    }
}

/// The first non-blank line of `stdout` merged with `stderr`.
///
/// Merged because tool output is chatty and the interesting line is on a
/// different stream per tool; first-line because only the first non-blank one
/// ever matters for a checklist.
fn first_line(stdout: &[u8], stderr: &[u8]) -> String {
    let merged = format!(
        "{}\n{}",
        String::from_utf8_lossy(stdout),
        String::from_utf8_lossy(stderr)
    );
    merged
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("(no output)")
        .to_string()
}

/// Run `program args…`, bounded, and keep only the first line.
async fn probe(program: &str, args: &[&str]) -> Probe {
    probe_within(program, args, PROBE_TIMEOUT).await
}

/// Run `program args…`, bounded by the caller's patience, and keep only the
/// first line.
///
/// `pub` because [`crate::runtime_health`] asks the same question of the same
/// tool on the dispatch path, with a tighter budget — one implementation with
/// a parameter, for the reason `doctor` reads `ImageFreshness` rather than
/// judging freshness a second time.
pub async fn probe_within(program: &str, args: &[&str], budget: Duration) -> Probe {
    let run = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output();
    match tokio::time::timeout(budget, run).await {
        Err(_) => Probe::TimedOut,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => Probe::Missing,
        Ok(Err(e)) => Probe::Failed(e.to_string()),
        Ok(Ok(out)) => Probe::Ran {
            ok: out.status.success(),
            text: first_line(&out.stdout, &out.stderr),
        },
    }
}

/// Run `program args…` and keep the whole of stdout — for output that is a
/// list rather than a verdict.
async fn probe_full(program: &str, args: &[&str]) -> Result<(bool, String), Probe> {
    let run = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output();
    match tokio::time::timeout(PROBE_TIMEOUT, run).await {
        Err(_) => Err(Probe::TimedOut),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => Err(Probe::Missing),
        Ok(Err(e)) => Err(Probe::Failed(e.to_string())),
        Ok(Ok(out)) => Ok((
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )),
    }
}

/// Where `name` is on `PATH`, walking `PATH` ourselves rather than shelling
/// out to `which` — one fewer external tool a diagnostic depends on, and
/// `which` is not guaranteed to be installed on a minimal host.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        true
    }
}

/// Total physical memory in MB, or `None` where it cannot be read.
///
/// `None` rather than `0`, deliberately: returning zero would fail the memory
/// ledger on every host running a platform we could not interrogate, which is
/// a diagnostic inventing a defect.
fn host_memory_mb() -> Option<u64> {
    if cfg!(target_os = "linux") {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
        let line = meminfo.lines().find(|l| l.starts_with("MemTotal:"))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        return Some(kb / 1024);
    }
    if cfg!(target_os = "macos") {
        let out = std::process::Command::new("/usr/sbin/sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()?;
        let bytes: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
        return Some(bytes / 1024 / 1024);
    }
    None
}

/// Free space in MB on the filesystem holding `path`.
///
/// It walks to the nearest **existing** ancestor first, because the directory
/// most worth asking about — the orchestrator's verify target — routinely does
/// not exist yet (it is created on first use), and "no such directory" is not
/// an answer to "is there room".
fn free_disk_mb(path: &Path) -> Option<u64> {
    let existing = path.ancestors().find(|p| p.exists())?;
    let out = std::process::Command::new("df")
        .arg("-Pk")
        .arg(existing)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let fields: Vec<&str> = text.lines().nth(1)?.split_whitespace().collect();
    // POSIX `df -P`: Filesystem, 1024-blocks, Used, Available, Capacity, Mount
    fields.get(3)?.parse::<u64>().ok().map(|kb| kb / 1024)
}

// ---------------------------------------------------------------------------
// The credential broker
// ---------------------------------------------------------------------------

/// What the advertised broker address answered.
///
/// The check exists because the broker is the one precondition where every
/// host-side signal reads healthy while no scout can run. Every credentialed
/// operation inside a Scout VM — the Anthropic traffic and the git clone both
/// — is redeemed against this listener, so a run with a valid lease, a healthy
/// pool, a present image and a good token still starts and dies if the VM
/// cannot reach it. That is not hypothetical: the macOS application firewall
/// severs a non-loopback listener whose binary it no longer recognises (a
/// `cargo clean` is enough), and it recovers on its own once the file is back.
///
/// **The probe must use the advertised address, not loopback.** During that
/// outage loopback answered a correct `401` while the bridge gateway accepted
/// the connection and returned nothing at all — so a `127.0.0.1` probe reads
/// as a pass at exactly the moment the thing is broken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrokerProbe {
    /// It spoke HTTP and demanded a lease. **This is the success condition**,
    /// which reads backwards to anyone skimming — hence [`Self::describe`]
    /// saying so out loud.
    DemandedLease,
    /// It spoke HTTP and said something else. Something is listening there;
    /// whether it is the broker is another question.
    SpokeHttp(u16),
    /// The connection opened and produced no HTTP — no bytes at all, or a
    /// reset before any arrived. The firewall case, and the one every other
    /// check on the machine is blind to.
    ///
    /// Anything that goes wrong *after* the connect lands here rather than in
    /// [`Self::Unreachable`], and that split is the whole point: a successful
    /// connect has already proved the address is reachable, so calling the
    /// failure that follows it "unreachable" would demote a `Fail` to a
    /// `Skip` — a skip sets no exit code, which is precisely the false
    /// negative this check exists to prevent.
    Silent(String),
    /// Nothing is listening on that address.
    Refused(String),
    /// The address could not be reached at all — which on a cold machine is
    /// the ordinary answer, since apple/container's bridge gateway does not
    /// exist until the first container has started.
    Unreachable(String),
}

/// Ask the broker's advertised address whether it is answering.
///
/// Written at the TCP level rather than through `reqwest` because the three
/// failures have to stay apart: "refused", "unreachable" and "accepted the
/// connection and sent nothing" are one HTTP-client error each and are three
/// different findings here.
pub(crate) async fn probe_broker(host: &str, port: u16) -> BrokerProbe {
    probe_broker_within(host, port, PROBE_TIMEOUT).await
}

/// [`probe_broker`] with the patience named by the caller.
///
/// `doctor` keeps the ten seconds a human running a diagnostic wants — the
/// most patient reading available. [`crate::broker_health`] runs the same probe
/// on the *dispatch* path, where a gate awaiting it stalls the tick, and asks
/// for three: a broker that has not answered in three seconds is not one a
/// clone is going to reach either. One implementation with a parameter rather
/// than two probes, for the reason `doctor` reads `ImageFreshness` instead of
/// judging freshness a second time.
pub async fn probe_broker_within(host: &str, port: u16, timeout: Duration) -> BrokerProbe {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let connect = tokio::net::TcpStream::connect((host, port));
    let stream = match tokio::time::timeout(timeout, connect).await {
        Err(_) => {
            return BrokerProbe::Unreachable(format!(
                "no answer from {host}:{port} in {}s",
                timeout.as_secs()
            ));
        }
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            return BrokerProbe::Refused(e.to_string());
        }
        Ok(Err(e)) => return BrokerProbe::Unreachable(e.to_string()),
        Ok(Ok(stream)) => stream,
    };
    let mut stream = stream;

    // Any broker route demands a lease before it does anything else, so an
    // unauthenticated request is a complete, side-effect-free question.
    let request = format!(
        "GET /anthropic/v1/models HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n"
    );
    let exchange = async {
        stream.write_all(request.as_bytes()).await?;
        stream.flush().await?;
        let mut buf = [0u8; 512];
        let read = stream.read(&mut buf).await?;
        Ok::<_, std::io::Error>(String::from_utf8_lossy(&buf[..read]).into_owned())
    };
    match tokio::time::timeout(timeout, exchange).await {
        Err(_) => BrokerProbe::Silent("it accepted the connection and never answered".into()),
        Ok(Err(e)) => BrokerProbe::Silent(e.to_string()),
        Ok(Ok(reply)) => classify_broker_reply(&reply),
    }
}

/// The classification half of [`probe_broker`], over bytes the caller already
/// has — so every verdict is testable against a real listener without needing
/// a firewall.
pub(crate) fn classify_broker_reply(reply: &str) -> BrokerProbe {
    let Some(status_line) = reply.lines().next().filter(|l| l.starts_with("HTTP/1.")) else {
        return BrokerProbe::Silent(match reply.is_empty() {
            true => "it returned no bytes at all".into(),
            false => "it answered something that is not HTTP".into(),
        });
    };
    match status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
    {
        Some(401) => BrokerProbe::DemandedLease,
        Some(code) => BrokerProbe::SpokeHttp(code),
        None => BrokerProbe::Silent("its status line carried no status code".into()),
    }
}

impl BrokerProbe {
    fn check(&self, host: &str, port: u16) -> Check {
        let name = "broker reachable";
        let fix = format!(
            "confirm the server is serving and that {host}:{port} is not being severed \
             (on macOS the application firewall drops a listener whose binary it no \
             longer recognises — re-approve `tasks`, or `make restart` to re-create it)"
        );
        match self {
            Self::DemandedLease => Check::ok(
                name,
                format!(
                    "{host}:{port} answered 401 `a lease is required` — which is the \
                     healthy answer, not a failure: every broker route demands a lease, \
                     so an unauthenticated 401 is what proves the listener is up"
                ),
            ),
            Self::SpokeHttp(code) => Check::warn(
                name,
                format!(
                    "{host}:{port} answered HTTP {code} rather than the 401 every broker \
                     route gives an unauthenticated request — something is listening \
                     there, but it may not be the broker"
                ),
                fix,
            ),
            Self::Silent(what) => Check::fail(
                name,
                format!(
                    "{host}:{port} accepted the connection and then {what} — no 401, which \
                     is what a healthy broker gives an unauthenticated request. This is the \
                     failure every other check on this machine is blind to: a scout redeems \
                     its Anthropic credit and its git clone here, so it would start and die"
                ),
                fix,
            ),
            Self::Refused(e) => Check::fail(
                name,
                format!("nothing is listening on {host}:{port} ({e})"),
                "start the server (`make restart`), which binds the broker as it boots",
            ),
            Self::Unreachable(e) => Check::skip(
                name,
                format!(
                    "{host}:{port} could not be reached ({e}) — on a cold machine this is \
                     the ordinary answer, since apple/container's bridge gateway does not \
                     exist until the first container has started"
                ),
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------

/// The image triple `make images` cross-compiles the supervisors for.
const IMAGE_TARGET: &str = "aarch64-unknown-linux-gnu";

/// Whether [`IMAGE_TARGET`] is this host's own triple.
///
/// It decides whether to ask about the cross linker at all. The Makefile pins
/// that linker unconditionally because cargo's `[target.*]` config keys are
/// host-blind — but on an aarch64 Linux host the image triple *is* the host
/// triple, and demanding a macOS-only linker there is a warning nobody can
/// clear.
fn image_target_is_host() -> bool {
    std::env::consts::ARCH == "aarch64" && std::env::consts::OS == "linux"
}

/// What `tasks doctor` was asked to do.
#[derive(Debug, Clone)]
pub struct DoctorOptions {
    pub data_dir: PathBuf,
    /// Whether a warning fails the run. Reported by [`Report::exit_code`]; the
    /// report itself is identical either way.
    pub strict: bool,
    /// Boot each VM image and read `--version` back — the cold read
    /// `make images-check` performs.
    ///
    /// Opt-in because it starts a container: not a write to any state the
    /// pipeline reads, but seconds per image, dependent on the container
    /// system being up, and a diagnostic whose cost scales with how broken the
    /// machine is is one people stop running. The default answers *presence*,
    /// which is what actually fails on a fresh machine, and names this read
    /// rather than performing it.
    pub probe_images: bool,
    /// What `.env` loading did, handed down from `main` rather than redone:
    /// the files were applied before the runtime started, and reading them a
    /// second time here could report a different answer than the one in force.
    pub env_sources: Vec<crate::env_file::Source>,
}

/// Ask everything, in the order the preconditions bite, and answer nothing
/// twice.
///
/// **Nothing short-circuits.** A failure that invalidates later checks emits
/// them as [`Level::Skip`] naming the reason, so the report has the same shape
/// on a broken machine as on a working one.
pub async fn run(opts: DoctorOptions) -> Report {
    let mut report = Report::default();

    // Custody is asked about *first* even though it prints further down.
    // `Config::from_env` opens the sealed store, and a store that exists and
    // cannot be unsealed is a hard error there — so building the config first
    // would collapse this whole report into the one line it exists to explain.
    let secrets = crate::secrets::Secrets::open(&opts.data_dir);
    let custody_error = secrets.as_ref().err().map(|e| e.to_string());
    let secrets = secrets.unwrap_or_else(|_| crate::secrets::Secrets::unresolvable());
    let config = crate::run::Config::from_env_with(opts.data_dir.clone(), secrets.clone());

    report.push(environment_section(&opts));
    report.push(configuration_section(&config));

    let container = container_section().await;
    let container_ok = container
        .checks
        .first()
        .is_some_and(|c| c.level == Level::Ok);
    report.push(container);
    report.push(toolchain_section().await);

    match config.as_ref() {
        Ok(config) => {
            report.push(vm_pool_section(config).await);
            let server = server_probe(config).await;
            report.push(server_section(config, &server));
            report.push(images_section(config, &server, container_ok, opts.probe_images).await);
            report.push(credentials_section(
                &opts,
                &secrets,
                custody_error.as_deref(),
            ));
            report.push(broker_section(config).await);
            report.push(github_section(config).await);
            report.push(projects_section(&server).await);
            report.push(orchestrator_section(config));
        }
        Err(_) => {
            let why = "the configuration does not parse, so nothing below it can be \
                       resolved from it";
            for title in [
                "vm-pool",
                "server",
                "images",
                "credentials",
                "credential broker",
                "github",
                "projects",
                "orchestrator",
            ] {
                let mut section = Section::new(title);
                section.push(Check::skip(title, why));
                report.push(section);
            }
        }
    }
    report
}

fn environment_section(opts: &DoctorOptions) -> Section {
    let mut section = Section::new("environment");

    // Empty sources have two meanings and they are different diagnoses, so
    // the switch is reported by name when it is set: it is *why* an
    // operator's `.env` is being ignored.
    let disabled = std::env::var("TASKS_ENV_FILES")
        .ok()
        .filter(|v| !v.trim().eq_ignore_ascii_case("on"));
    match (disabled, opts.env_sources.as_slice()) {
        (Some(value), _) => section.push(Check::note(
            ".env",
            format!("not loaded: TASKS_ENV_FILES={value}"),
        )),
        (None, []) => section.push(Check::ok(
            ".env",
            "none found; configuration comes from the environment alone",
        )),
        (None, sources) => {
            for source in sources {
                let check = match &source.error {
                    Some(error) => Check::fail(
                        ".env",
                        format!("{}: {error}", source.path.display()),
                        format!("fix the syntax in {}", source.path.display()),
                    ),
                    // Names only, never values: a `.env` is mostly secrets.
                    None => Check::ok(
                        ".env",
                        format!(
                            "{} set {}{}",
                            source.path.display(),
                            if source.applied.is_empty() {
                                "nothing".to_string()
                            } else {
                                source.applied.join(", ")
                            },
                            if source.shadowed.is_empty() {
                                String::new()
                            } else {
                                format!(" (ignored, already set: {})", source.shadowed.join(", "))
                            }
                        ),
                    ),
                };
                section.push(check);
            }
        }
    }

    let dir = &opts.data_dir;
    if dir.is_dir() {
        section.push(Check::ok("data dir", dir.display().to_string()));
        section.push(write_probe(dir));
    } else {
        section.push(Check::warn(
            "data dir",
            format!("{} does not exist yet", dir.display()),
            format!("mkdir -p {}", dir.display()),
        ));
        section.push(Check::skip(
            "data dir writable",
            "the directory does not exist yet",
        ));
    }
    section
}

/// The one deliberate write this command makes.
///
/// Writability is only answerable by writing: mode bits lie under ACLs, a
/// read-only mount and a full disk. One uniquely-named file, created and
/// removed, on no path this system reads.
fn write_probe(dir: &Path) -> Check {
    let probe = dir.join(format!(".doctor-write-probe-{}", uuid::Uuid::new_v4()));
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Check::ok("data dir writable", "yes")
        }
        Err(e) => Check::fail(
            "data dir writable",
            format!("cannot create a file in {}: {e}", dir.display()),
            format!(
                "chown/chmod {} so this user can write it, or point TASKS_DATA_DIR \
                 somewhere writable",
                dir.display()
            ),
        ),
    }
}

fn configuration_section(config: &Result<crate::run::Config, crate::run::ConfigError>) -> Section {
    let mut section = Section::new("configuration");
    match config {
        Ok(config) => {
            section.push(Check::ok(
                "parses",
                format!(
                    "port {}, poll every {}s, {} scout(s) at once",
                    config.port,
                    config.poll_interval.as_secs(),
                    config.scout_max_concurrent
                ),
            ));
            section.push(Check::ok(
                "budgets",
                format!(
                    "scout {}s, build {}s, orchestrator {}s",
                    config.scout_timeout.as_secs(),
                    config.builder_timeout.as_secs(),
                    config.orchestrator_timeout.as_secs()
                ),
            ));
        }
        // A server would refuse to boot on this; without the check the
        // failure arrives at `serve` time instead.
        Err(e) => section.push(Check::fail(
            "parses",
            e.to_string(),
            "correct the named variable in .env or the environment",
        )),
    }
    section
}

/// The container runtime, which everything below it depends on.
///
/// The subcommand spellings are apple/container's own: `container system
/// status` ("checks whether the container services are running") and
/// `container image list` — **singular `image`**, whose alias is `ls`. Both
/// failure paths degrade honestly, so a spelling that is nonetheless wrong on
/// some future version produces a wrong *message* and never a false pass.
async fn container_section() -> Section {
    let mut section = Section::new("container runtime");
    let Some(path) = which("container") else {
        section.push(Check::fail(
            "container CLI",
            "not on PATH",
            "install apple/container (https://github.com/apple/container) — the whole \
             pipeline runs in VMs it starts",
        ));
        section.push(Check::skip(
            "container services",
            "there is no container CLI",
        ));
        return section;
    };

    // The version is a nicety; presence is the finding. A CLI that will not
    // report one is not a failure, so this never fails on the version alone.
    let version = probe("container", &["--version"]).await;
    let detail = match version.succeeded() {
        true => format!("{} ({})", path.display(), version.text()),
        false => path.display().to_string(),
    };
    section.push(Check::ok("container CLI", detail));

    let status = probe("container", &["system", "status"]).await;
    section.push(match &status {
        Probe::Ran { ok: true, text } => Check::ok("container services", text.clone()),
        Probe::TimedOut => Check::skip(
            "container services",
            "`container system status` did not answer in 10s",
        ),
        other => Check::fail(
            "container services",
            format!("`container system status` says: {}", other.text()),
            "container system start",
        ),
    });
    section
}

/// What `make images` needs, asked here so the report closes a loop rather
/// than opening one: several findings below name `make images` as their fix,
/// and a report that hands over a command which fails on its next line for a
/// third reason nobody mentioned has not helped.
///
/// Every check here is a **warning, never a failure**. A host whose images are
/// already built runs scouts perfectly well with none of this installed —
/// which is every machine that received a bundle rather than a checkout.
async fn toolchain_section() -> Section {
    let mut section = Section::new("build toolchain (only needed to run `make images`)");

    let targets = probe_full("rustup", &["target", "list", "--installed"]).await;
    section.push(match targets {
        Ok((true, list)) if list.lines().any(|l| l.trim() == IMAGE_TARGET) => {
            Check::ok("rust target", IMAGE_TARGET)
        }
        Ok((true, _)) => Check::warn(
            "rust target",
            format!("{IMAGE_TARGET} is not installed"),
            format!("rustup target add {IMAGE_TARGET}"),
        ),
        Ok((false, out)) => Check::skip("rust target", format!("rustup refused: {out}")),
        Err(Probe::Missing) => Check::skip("rust target", "rustup is not installed"),
        Err(other) => Check::skip("rust target", other.text().to_string()),
    });

    if image_target_is_host() {
        section.push(Check::ok(
            "cross linker",
            format!("not needed: {IMAGE_TARGET} is this host's own triple"),
        ));
    } else {
        let linker = format!("{IMAGE_TARGET}-gcc");
        section.push(match which(&linker) {
            Some(path) => Check::ok("cross linker", path.display().to_string()),
            None => Check::warn(
                "cross linker",
                format!("{linker} is not on PATH"),
                format!("brew install messense/macos-cross-toolchains/{IMAGE_TARGET}"),
            ),
        });
    }

    section.push(match which("cargo-nextest") {
        Some(path) => Check::ok("cargo-nextest", path.display().to_string()),
        None => Check::warn(
            "cargo-nextest",
            "not installed; `make test` needs it (`make test-cargo` does not)",
            "cargo install cargo-nextest --locked",
        ),
    });
    section
}

async fn vm_pool_section(config: &crate::run::Config) -> Section {
    use tasks_protocol::TasksProtocol;
    use vm_pool_client::Client;

    let mut section = Section::new("vm-pool");
    let socket = &config.vm_pool_socket;

    let connected = tokio::time::timeout(PROBE_TIMEOUT, Client::<TasksProtocol>::connect(socket))
        .await
        .map_err(|_| "the connect did not complete in 10s".to_string())
        .and_then(|r| r.map_err(|e| e.to_string()));
    let client = match connected {
        Ok(client) => {
            section.push(Check::ok("socket", socket.display().to_string()));
            client
        }
        Err(e) => {
            section.push(Check::fail(
                "socket",
                format!("nothing answering on {}: {e}", socket.display()),
                "start it with `tasks vm-pool` (the server autospawns one only when it \
                 is an installed binary — see TASKS_VM_POOL_AUTOSPAWN)",
            ));
            for name in ["protocol", "slot ledger", "memory ledger"] {
                section.push(Check::skip(name, "vm-pool is not answering"));
            }
            return section;
        }
    };

    let status = tokio::time::timeout(PROBE_TIMEOUT, client.handle().status())
        .await
        .map_err(|_| "`status` did not answer in 10s".to_string())
        .and_then(|r| r.map_err(|e| e.to_string()));
    let status = match status {
        Ok(status) => status,
        Err(e) => {
            section.push(Check::fail(
                "protocol",
                format!("`status` is the oldest command there is and it failed: {e}"),
                "restart `tasks vm-pool`",
            ));
            for name in ["slot ledger", "memory ledger"] {
                section.push(Check::skip(name, "vm-pool would not report its status"));
            }
            return section;
        }
    };

    // `AttachSupport` already names its own fix; rendering it verbatim beats
    // summarizing it into a verdict the reader then translates back.
    let support = crate::reattach::support_of(&status);
    section.push(if support.is_supported() {
        Check::ok("protocol", support.to_string())
    } else {
        Check::warn(
            "protocol",
            format!("{support} — a restart from here would write off work in flight"),
            "restart `tasks vm-pool` from this build",
        )
    });

    // The classification *and* its severity come off `Capacity`, so this and
    // the connect-time log line cannot disagree about what an exactly-sized
    // pool means.
    let capacity = crate::run::Capacity::assess(status.total, config.scout_max_concurrent);
    let detail = format!(
        "{} ({} allocated, {} free right now)",
        capacity.describe(),
        status.allocated,
        status.available
    );
    section.push(match (capacity.level(), capacity.fix()) {
        (Level::Fail, Some(fix)) => Check::fail("slot ledger", detail, fix),
        (Level::Warn, Some(fix)) => Check::warn("slot ledger", detail, fix),
        _ => Check::ok("slot ledger", detail),
    });

    let wanted = crate::run::memory_reserve_mb(
        config.scout_max_concurrent,
        config.vm_config.memory_mb.unwrap_or(0),
        config.builder_vm_config.memory_mb.unwrap_or(0),
    );
    let arithmetic = format!(
        "{} scout(s) x {} MB + a builder at {} MB + {} MB for buildkit = {} MB reserved",
        config.scout_max_concurrent,
        config.vm_config.memory_mb.unwrap_or(0),
        config.builder_vm_config.memory_mb.unwrap_or(0),
        crate::run::BUILDKIT_RESERVE_MB,
        wanted
    );
    section.push(match host_memory_mb() {
        // Unknown, with the arithmetic still printed: a machine we could not
        // interrogate has not been shown to be too small.
        None => Check::skip(
            "memory ledger",
            format!("{arithmetic}; this host's total memory could not be read"),
        ),
        Some(host) if host >= wanted => {
            Check::ok("memory ledger", format!("{arithmetic}, of {host} MB"))
        }
        Some(host) => Check::warn(
            "memory ledger",
            format!("{arithmetic}, but this host has {host} MB"),
            "lower SCOUT_MAX_CONCURRENT, or SCOUT_VM_MEMORY_MB / BUILDER_VM_MEMORY_MB",
        ),
    });
    section
}

/// One round of questions to whatever is serving, asked once and shared by the
/// four sections that need an answer from it.
struct ServerProbe {
    file: Option<tasks_api::paths::PidFile>,
    status: Option<tasks_api::http::ServerStatus>,
    version: Option<tasks_api::version::VersionInfo>,
    /// Why `/status` did not answer, when there was a live pid to ask.
    error: Option<String>,
}

impl ServerProbe {
    fn port(&self) -> Option<u16> {
        self.file.as_ref().map(|f| f.port)
    }
}

async fn server_probe(config: &crate::run::Config) -> ServerProbe {
    let file = crate::pidfile::read_live(&config.data_dir);
    let Some(port) = file.as_ref().map(|f| f.port) else {
        return ServerProbe {
            file,
            status: None,
            version: None,
            error: None,
        };
    };
    let (status, error) = match crate::reload::fetch_status(port).await {
        Ok(status) => (Some(status), None),
        Err(e) => (None, Some(e)),
    };
    ServerProbe {
        file,
        // `ServerStatus` carries no version, so the binary comparison needs a
        // second call. Plain `reqwest`, because `reload` only exposes a
        // `/status` fetch.
        version: fetch_json(port, "/version").await,
        status,
        error,
    }
}

/// `GET http://127.0.0.1:<port><path>`, decoded, or `None`.
///
/// Loopback and read-only: the API binds loopback deliberately, and every
/// route this asks for is a `GET`.
async fn fetch_json<T: serde::de::DeserializeOwned>(port: u16, path: &str) -> Option<T> {
    let client = reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .build()
        .ok()?;
    client
        .get(format!("http://127.0.0.1:{port}{path}"))
        .send()
        .await
        .ok()?
        .json::<T>()
        .await
        .ok()
}

fn server_section(config: &crate::run::Config, probe: &ServerProbe) -> Section {
    let mut section = Section::new("server");

    let Some(file) = &probe.file else {
        // Not serving is a warning and not a failure: it is a fact about the
        // choices made on this machine, and every check that needed a server
        // says so for itself.
        section.push(Check::warn(
            "serving",
            format!(
                "nothing is serving (no live pidfile under {})",
                config.data_dir.display()
            ),
            "start it: `make restart`, or `tasks service install` for a managed one",
        ));
        for name in ["build", "mode", "dispatch holds"] {
            section.push(Check::skip(name, "nothing is serving"));
        }
        return section;
    };

    let Some(status) = &probe.status else {
        section.push(Check::fail(
            "serving",
            format!(
                "pid {} is alive on port {} but is not answering /status{}",
                file.pid,
                file.port,
                probe
                    .error
                    .as_deref()
                    .map(|e| format!(" ({e})"))
                    .unwrap_or_default()
            ),
            "`tasks stop` then `make restart`",
        ));
        for name in ["build", "mode", "dispatch holds"] {
            section.push(Check::skip(name, "the server is not answering /status"));
        }
        return section;
    };

    section.push(Check::ok(
        "serving",
        format!(
            "pid {} on port {} ({})",
            status.pid,
            file.port,
            file.exe.display()
        ),
    ));

    section.push(match &probe.version {
        None => Check::skip("build", "/version did not answer"),
        Some(info) if info.version == crate::version::VERSION => {
            Check::ok("build", format!("{} ({})", info.version, info.commit))
        }
        Some(info) => Check::warn(
            "build",
            format!(
                "serving {} ({}); this binary is {} ({})",
                info.version,
                info.commit,
                crate::version::VERSION,
                crate::version::COMMIT
            ),
            "make restart",
        ),
    });

    section.push(match status.mode {
        tasks_api::models::Mode::Play => Check::ok("mode", "play — new work dispatches"),
        // Deliberately set not to run is a warning, not a defect.
        mode => Check::warn(
            "mode",
            format!("{} — no new scout or build starts", mode.as_str()),
            "tasks resume",
        ),
    });

    // Each hold already names its own discharge, so they are rendered rather
    // than summarized. Absence of all five is the ordinary case.
    let mut holds = Vec::new();
    if let Some(github) = &status.github {
        holds.push(format!(
            "github not answering since {}: {}",
            github.since, github.error
        ));
    }
    if let Some(update) = &status.update {
        holds.push(format!("update pending: {}", update.reasons.join("; ")));
    }
    if let Some(pool) = &status.pool {
        holds.push(format!("vm-pool full (0 of {})", pool.total));
    }
    if let Some(broker) = &status.broker {
        holds.push(format!(
            "broker at {} not answering since {}: {}",
            broker.address, broker.since, broker.error
        ));
    }
    if let Some(runtime) = &status.runtime {
        holds.push(format!(
            "container runtime down since {}: {}",
            runtime.since, runtime.error
        ));
    }
    section.push(match holds.is_empty() {
        true => Check::ok("dispatch holds", "none"),
        false => Check::warn(
            "dispatch holds",
            holds.join(" | "),
            "each hold clears on its own terms: a GitHub hold on the next successful \
             poll, an update hold on `make restart` / `make images`, a pool hold on the \
             next VM handed back, a broker hold on the next probe that gets a 401, a \
             runtime hold on `container system start` (the `broker reachable` and \
             `container services` checks above are the same two questions, asked here \
             directly)",
        ),
    });
    section
}

/// Whether a `container image list` listing holds `name:tag`.
///
/// Two fields, NAME and TAG, or a single `name:tag` token — apple/container's
/// column layout is not documented, and both shapes are plausible. What this
/// deliberately does *not* do is match a substring: the reference `agent:v1`
/// appears nowhere in a table whose columns are split, so a substring test
/// would report every image as missing. This is the single function to change
/// if the real layout is a third thing.
fn lists_image(listing: &str, name: &str, tag: &str) -> bool {
    let joined = format!("{name}:{tag}");
    listing.lines().any(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        match fields.as_slice() {
            [first, second, ..] => (*first == name && *second == tag) || *first == joined,
            [only] => *only == joined,
            [] => false,
        }
    })
}

async fn images_section(
    config: &crate::run::Config,
    probe: &ServerProbe,
    container_ok: bool,
    probe_images: bool,
) -> Section {
    use tasks_api::version::ImageFreshness;

    let mut section = Section::new("VM images");
    let images = [
        ("scout image", config.scout_image.as_str()),
        ("builder image", config.builder_image.as_str()),
    ];

    let listing = match container_ok {
        false => Err("there is no working container CLI to ask".to_string()),
        true => match probe_full("container", &["image", "list"]).await {
            Ok((true, out)) => Ok(out),
            Ok((false, out)) => Err(format!("`container image list` refused: {out}")),
            Err(other) => Err(other.text().to_string()),
        },
    };

    for (name, reference) in images {
        let (image, tag) = reference.split_once(':').unwrap_or((reference, "latest"));
        section.push(match &listing {
            Err(why) => Check::skip(name, why.clone()),
            Ok(listing) if lists_image(listing, image, tag) => Check::ok(name, reference),
            Ok(_) => Check::fail(
                name,
                format!("{reference} is not built on this host"),
                "make images",
            ),
        });
    }

    // The observed stamp is free but only exists once a run has started
    // inside an image, which on the machine this command is for has never
    // happened. Nothing observed is *not* a clean bill of health.
    match probe.status.as_ref().map(|s| s.images.as_slice()) {
        None => section.push(Check::skip(
            "observed identity",
            "nothing is serving, so no run's reading of an image can be read back",
        )),
        Some([]) => section.push(Check::note(
            "observed identity",
            "none observed yet — an image's identity is only ever reported by a run that \
             started inside it, and none has since this server booted",
        )),
        Some(observed) => {
            for identity in observed {
                let detail = format!(
                    "{} {} ({})",
                    identity.image,
                    identity.version.as_deref().unwrap_or("no version reported"),
                    identity.freshness.as_str()
                );
                // The verdict is read off `ImageFreshness`, never re-decided
                // here: `needs_rebuild` is the one predicate, and `Unstamped`
                // is the loudest reading it has rather than an unknown.
                section.push(match identity.freshness.needs_rebuild() {
                    true => Check::fail("observed identity", detail, "make images"),
                    false => Check::ok("observed identity", detail),
                });
            }
            if observed
                .iter()
                .any(|i| i.freshness == ImageFreshness::Unknown)
            {
                section.push(Check::note(
                    "observed identity",
                    "an image reported a version that does not parse — an unidentifiable \
                     build, not a stale one",
                ));
            }
        }
    }

    if !probe_images {
        section.push(Check::ok(
            "cold read",
            "not performed (it boots a container); `tasks doctor --probe-images` reads \
             each image's own --version, as `make images-check` does",
        ));
        return section;
    }
    for (name, reference) in images {
        // Without a working CLI the cold read cannot be *made*, which is a
        // skip: reporting it as a stale image would blame the image for the
        // container runtime's absence, already reported above.
        if !container_ok {
            section.push(Check::skip(
                format!("{name} (cold)"),
                "there is no working container CLI to boot it with",
            ));
            continue;
        }
        let read = probe_within(
            "container",
            &["run", "--rm", reference, "--version"],
            IMAGE_PROBE_TIMEOUT,
        )
        .await;
        section.push(match &read {
            Probe::Ran { ok: true, text } => {
                let version = text.split_whitespace().nth(1);
                let freshness = ImageFreshness::judge(version, crate::version::VERSION);
                let detail = format!("{reference}: {text} ({})", freshness.as_str());
                match freshness.needs_rebuild() {
                    true => Check::fail(format!("{name} (cold)"), detail, "make images"),
                    false => Check::ok(format!("{name} (cold)"), detail),
                }
            }
            // An image that reports no identity predates stamping, and is
            // therefore older than any version it could have named.
            other => Check::fail(
                format!("{name} (cold)"),
                format!("{reference} would not report a version: {}", other.text()),
                "make images",
            ),
        });
    }
    section
}

fn credentials_section(
    opts: &DoctorOptions,
    secrets: &crate::secrets::Secrets,
    custody_error: Option<&str>,
) -> Section {
    use crate::secrets::SecretName;

    let mut section = Section::new("credentials");

    // `status` reads the store header and needs no unseal key — which is the
    // whole point: it has to work exactly when the key is what is missing.
    let store = crate::secrets::status(&opts.data_dir);
    match &store {
        Ok(status) => {
            let names: Vec<&str> = status.entries.iter().map(|e| e.name.as_str()).collect();
            section.push(Check::ok(
                "sealed store",
                format!(
                    "{} holds {}",
                    status.path.display(),
                    if names.is_empty() {
                        "no entries".to_string()
                    } else {
                        names.join(", ")
                    }
                ),
            ));
            section.push(Check::ok("unseal key source", status.key_source.clone()));
        }
        Err(crate::secrets::SecretsError::NotInitialized(path)) => {
            section.push(Check::note(
                "sealed store",
                format!(
                    "none at {} — the environment fallbacks are what this host runs on",
                    path.display()
                ),
            ));
            section.push(Check::skip("unseal key source", "there is no sealed store"));
        }
        Err(e) => {
            section.push(Check::fail(
                "sealed store",
                e.to_string(),
                "repair or re-create it: `tasks secrets init` writes a new one (the old \
                 entries are not recoverable without the key that sealed them)",
            ));
            section.push(Check::skip(
                "unseal key source",
                "the store header is unreadable",
            ));
        }
    }

    if let Some(error) = custody_error {
        // A store that exists and will not open is a hard boot error for the
        // server, so this is the finding the two skips below point at.
        section.push(Check::fail(
            "unseal key",
            format!("the sealed store will not open: {error}"),
            format!(
                "make the unseal key reachable — it lives in this host's credential \
                 store, or in the file named by {}",
                crate::secrets::KEY_FILE_ENV
            ),
        ));
    } else if store.is_ok() {
        section.push(Check::ok("unseal key", "the sealed store opened"));
    }

    for name in SecretName::ALL {
        let label = format!("{name}");
        // "Nothing resolves" and "we cannot tell" are the same observation and
        // opposite advice: the sealed entries may well hold both keys, and
        // reporting them absent sends an operator to re-seal what is already
        // sealed. The real fix is the failure above.
        if custody_error.is_some() {
            section.push(Check::skip(
                label,
                "the sealed store could not be opened, so what it holds is unknown",
            ));
            continue;
        }
        // Never the value — `CredentialSource` has none in it.
        section.push(match secrets.source_of(name) {
            Some(source) => Check::ok(label, format!("resolves from {source}")),
            None if name == SecretName::GithubToken => Check::fail(
                label,
                "nothing resolves it: no sealed entry, no GITHUB_TOKEN",
                "tasks secrets set github-token",
            ),
            None => Check::fail(
                label,
                "nothing resolves it: no sealed entry, no ANTHROPIC_API_KEY, no \
                 ~/.claude/anthropic_key.sh",
                "tasks secrets set anthropic-api-key",
            ),
        });
    }
    section
}

/// The path a VM redeems its lease on — see [`BrokerProbe`] for why this is
/// asked at the advertised address and why a 401 is the pass.
async fn broker_section(config: &crate::run::Config) -> Section {
    let mut section = Section::new("credential broker");
    let (host, port) = (config.broker.advertise_host.as_str(), config.broker.port);
    section.push(Check::ok(
        "advertised address",
        format!("{host}:{port} — what a Scout VM's ANTHROPIC_BASE_URL and clone URL point at"),
    ));
    section.push(probe_broker(host, port).await.check(host, port));
    section
}

async fn github_section(config: &crate::run::Config) -> Section {
    let mut section = Section::new("github");

    if !config.github_configured() {
        section.push(Check::fail(
            "token",
            "no GitHub credential resolves, so intake, clones and every write are off",
            "tasks secrets set github-token",
        ));
        section.push(Check::skip("identity", "there is no token to ask about"));
        section.push(Check::skip("scopes", "there is no token to ask about"));
        return section;
    }

    let client = crate::github::GitHubClient::from_secrets(
        config.secrets.clone(),
        config.github_api_url.as_deref(),
    );
    let viewer = match tokio::time::timeout(PROBE_TIMEOUT, client.viewer()).await {
        Err(_) => {
            section.push(Check::skip("identity", "GitHub did not answer in 10s"));
            section.push(Check::skip("scopes", "GitHub did not answer"));
            return section;
        }
        Ok(Ok(viewer)) => viewer,
        // Structural, never off the message text: `is_unavailable` is 5xx or
        // no answer at all, and everything else is GitHub *answering* — which
        // for a credential means it was rejected.
        Ok(Err(e)) if e.is_unavailable() => {
            section.push(Check::skip(
                "identity",
                format!("GitHub is not answering: {e}"),
            ));
            section.push(Check::skip("scopes", "GitHub is not answering"));
            return section;
        }
        Ok(Err(e)) => {
            section.push(Check::fail(
                "identity",
                format!("GitHub rejected this token: {e}"),
                "tasks secrets set github-token, with a token that works",
            ));
            section.push(Check::skip("scopes", "the token was rejected"));
            return section;
        }
    };

    section.push(Check::ok(
        "identity",
        format!("authenticates as {}", viewer.login),
    ));
    section.push(match (&viewer.scopes, viewer.scope_source) {
        (Some(scopes), Some(source)) if scopes.iter().any(|s| s == "repo") => Check::ok(
            "scopes",
            format!("{} (from the {source})", scopes.join(", ")),
        ),
        (Some(scopes), Some(source)) if scopes.is_empty() => Check::fail(
            "scopes",
            format!("this token carries no scopes at all (the {source} was empty)"),
            "issue a classic token with `repo`, or a fine-grained one with contents, \
             issues and pull-requests write",
        ),
        (Some(scopes), Some(source)) => Check::warn(
            "scopes",
            format!(
                "{} (from the {source}) — no `repo`, so clones of a private repository \
                 and PR writes will fail",
                scopes.join(", ")
            ),
            "re-issue the token with the `repo` scope",
        ),
        // Absent is not empty. A fine-grained PAT or a GitHub App token has
        // permissions rather than scopes and sends no header at all; reading
        // that as "no scopes" would tell an operator to replace a token that
        // works.
        _ => Check::note(
            "scopes",
            "not enumerable from here — neither the GraphQL response nor GET /rate_limit \
             carried x-oauth-scopes, which is what a fine-grained PAT or a GitHub App \
             token does. Its permissions are set where it was issued",
        ),
    });
    section
}

async fn projects_section(probe: &ServerProbe) -> Section {
    let mut section = Section::new("projects");
    let Some(port) = probe.port().filter(|_| probe.status.is_some()) else {
        // The store is never opened here — `Store::open` migrates — so a host
        // with no server genuinely cannot answer this.
        section.push(Check::skip(
            "tracked",
            "nothing is serving, and doctor never opens the store (Store::open runs \
             migrations)",
        ));
        return section;
    };
    let Some(projects) = fetch_json::<Vec<tasks_api::models::Project>>(port, "/projects").await
    else {
        section.push(Check::skip("tracked", "/projects did not answer"));
        return section;
    };
    let active: Vec<String> = projects
        .iter()
        .filter(|p| p.status == tasks_api::models::ProjectStatus::Active)
        .map(|p| p.slug())
        .collect();
    section.push(match (projects.len(), active.len()) {
        (0, _) => Check::warn(
            "tracked",
            "no repository is tracked, so there is nothing for a scout to work on",
            "tasks add-project owner/repo",
        ),
        (_, 0) => Check::warn(
            "tracked",
            format!(
                "{} repository/ies tracked, none active — a paused or archived project \
                 dispatches nothing",
                projects.len()
            ),
            "POST /projects/{id}/status with `active`",
        ),
        _ => Check::ok("tracked", format!("active: {}", active.join(", "))),
    });
    section
}

fn orchestrator_section(config: &crate::run::Config) -> Section {
    let mut section = Section::new("orchestrator");

    section.push(Check::ok("command", config.orchestrator_cmd.clone()));

    // `workdir_is_checkout` in the *prompt* is `orchestrator_workdir.is_some()`
    // — so pointing ORCHESTRATOR_WORKDIR at any directory makes the system
    // prompt promise the agent a repository it does not have. This warns about
    // the discrepancy rather than fixing it: the fix is a change to the prompt
    // generator and belongs in its own commit.
    match &config.orchestrator_workdir {
        None => section.push(Check::ok(
            "workdir",
            format!(
                "{} (the neutral default — the orchestrator runs curl-only)",
                config.data_dir.join("orchestrator").display()
            ),
        )),
        Some(dir) if crate::reload::workspace_above(dir).is_some() => section.push(Check::ok(
            "workdir",
            format!("{} (a checkout)", dir.display()),
        )),
        Some(dir) => section.push(Check::warn(
            "workdir",
            format!(
                "{} is not a checkout, but the system prompt derives \
                 `workdir_is_checkout` from ORCHESTRATOR_WORKDIR merely being set — so \
                 the orchestrator is being promised a repository it does not have",
                dir.display()
            ),
            "point ORCHESTRATOR_WORKDIR at a repo checkout, or unset it",
        )),
    }

    let target = config.orchestrator_target_dir();
    // A warning, never a failure: this is what a merge decision's verification
    // rests on, and a machine that cannot hold it silently drops the
    // orchestrator back to a typecheck — but it is not a scout precondition.
    section.push(match free_disk_mb(&target) {
        None => Check::skip(
            "verify target dir",
            format!("{}: free space could not be read", target.display()),
        ),
        Some(free) if free >= 8192 => Check::ok(
            "verify target dir",
            format!(
                "{} ({free} MB free; one warm workspace build measured 3.2 GB, and the \
                 directory is bounded by ORCHESTRATOR_TARGET_BUDGET_GB — its current \
                 size is on `tasks status`)",
                target.display()
            ),
        ),
        Some(free) => Check::warn(
            "verify target dir",
            format!(
                "{} has {free} MB free; one warm workspace build measured 3.2 GB, and \
                 without room for it the orchestrator's verification drops back to a \
                 typecheck",
                target.display()
            ),
            "free space, or point ORCHESTRATOR_TARGET_DIR at a roomier filesystem",
        ),
    });
    section
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report_of(checks: Vec<Check>) -> Report {
        let mut section = Section::new("test");
        for check in checks {
            section.push(check);
        }
        let mut report = Report::default();
        report.push(section);
        report
    }

    #[test]
    fn a_clean_report_exits_zero_in_both_modes() {
        let report = report_of(vec![Check::ok("a", "fine"), Check::ok("b", "fine")]);
        assert_eq!(report.exit_code(false), 0);
        assert_eq!(report.exit_code(true), 0);
    }

    #[test]
    fn a_failure_exits_one_in_both_modes() {
        let report = report_of(vec![Check::ok("a", "fine"), Check::fail("b", "no", "do x")]);
        assert_eq!(report.exit_code(false), 1);
        assert_eq!(report.exit_code(true), 1);
    }

    /// A warning is a fact about the choices made on the machine, so it does
    /// not fail the run — unless the caller asked for that reading.
    #[test]
    fn a_warning_fails_only_under_strict() {
        let report = report_of(vec![Check::warn("a", "degraded", "do x")]);
        assert_eq!(report.exit_code(false), 0);
        assert_eq!(report.exit_code(true), 1);
    }

    /// The skip rule. Every skip has a failure above it that caused it, and
    /// that failure is what fails the run; a skip that failed too would report
    /// one broken thing as two, and `--strict` on a machine with no container
    /// CLI would be unreadable.
    #[test]
    fn a_skip_never_sets_the_exit_code() {
        let report = report_of(vec![
            Check::ok("a", "fine"),
            Check::skip("b", "could not ask"),
            Check::skip("c", "could not ask"),
        ]);
        assert_eq!(report.exit_code(false), 0);
        assert_eq!(
            report.exit_code(true),
            0,
            "a skip is not a warning: it is a question that was not asked"
        );
    }

    /// The signature guarantee, checked at the rendering layer too: a reader
    /// who sees a complaint sees the command that answers it, on the next
    /// line.
    #[test]
    fn every_failure_renders_its_fix() {
        let report = report_of(vec![Check::fail("thing", "is broken", "run the thing")]);
        let rendered = report.to_string();
        assert!(rendered.contains("is broken"), "{rendered}");
        assert!(rendered.contains("-> run the thing"), "{rendered}");
        for check in report.checks().filter(|c| c.level == Level::Fail) {
            assert!(check.fix.is_some(), "a Fail without a fix: {check:?}");
        }
    }

    /// `note` is the *named* escape hatch, so a fixless warning is always a
    /// deliberate one rather than a forgotten one.
    #[test]
    fn only_note_makes_a_fixless_warning() {
        assert!(Check::warn("a", "b", "c").fix.is_some());
        assert!(Check::note("a", "b").fix.is_none());
        assert_eq!(Check::note("a", "b").level, Level::Warn);
    }

    #[test]
    fn is_bad_is_true_for_warn_and_fail_only() {
        assert!(Level::Warn.is_bad());
        assert!(Level::Fail.is_bad());
        assert!(!Level::Ok.is_bad());
        assert!(!Level::Skip.is_bad(), "a skip is not something to act on");
    }

    /// The severity that both the connect-time log line and this command read.
    /// Stub `Capacity::level` and this is what goes red.
    #[test]
    fn capacity_severity_is_read_from_one_place() {
        use crate::run::Capacity;
        assert_eq!(Capacity::assess(3, 4).level(), Level::Fail);
        assert_eq!(Capacity::assess(3, 2).level(), Level::Warn);
        assert_eq!(Capacity::assess(6, 3).level(), Level::Ok);
        // And the two bad levels carry a command, exactly as `Check::fail`
        // and `Check::warn` will demand of them.
        assert!(Capacity::assess(3, 4).fix().is_some());
        assert!(Capacity::assess(3, 2).fix().is_some());
        assert!(Capacity::assess(6, 3).fix().is_none());
    }

    /// NAME and TAG as two fields, or one `name:tag` token — and never a
    /// substring, because `agent:v1` appears nowhere in a split table and a
    /// substring test would report every image as missing.
    #[test]
    fn image_listing_matches_name_and_tag_and_never_a_substring() {
        let table = "NAME    TAG    DIGEST\nagent   v1     sha256:abc\nbuilder v1     sha256:def\n";
        assert!(lists_image(table, "agent", "v1"));
        assert!(lists_image(table, "builder", "v1"));
        assert!(!lists_image(table, "agent", "v2"));
        assert!(!lists_image(table, "scout", "v1"));

        let joined = "agent:v1\nbuilder:v1\n";
        assert!(lists_image(joined, "agent", "v1"));
        assert!(!lists_image(joined, "agent", "v2"));

        // The failure a substring match would produce in reverse: a row for a
        // *different* image whose name contains ours must not match.
        assert!(!lists_image("NAME TAG\nmyagent v1\n", "agent", "v1"));
    }

    /// The one deliberate write, and the promise that bounds it.
    #[test]
    fn the_write_probe_leaves_the_directory_as_it_found_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep-me"), b"hello").unwrap();
        let before: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();

        let check = write_probe(dir.path());
        assert_eq!(check.level, Level::Ok, "{check:?}");

        let after: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(before, after, "the probe left something behind");
    }

    #[test]
    fn the_write_probe_fails_with_a_fix_where_it_cannot_write() {
        let check = write_probe(Path::new("/proc/definitely/not/writable"));
        assert_eq!(check.level, Level::Fail);
        assert!(check.fix.is_some());
    }

    #[test]
    fn which_finds_an_executable_on_path_and_not_a_missing_one() {
        assert!(which("sh").is_some(), "every unix host has a shell");
        assert!(which("tasks-doctor-no-such-binary-9f3a").is_none());
    }

    #[tokio::test]
    async fn probes_report_missing_refused_and_ran_apart() {
        assert_eq!(
            probe("tasks-doctor-no-such-binary-9f3a", &[]).await,
            Probe::Missing
        );
        let ran = probe("sh", &["-c", "echo hello"]).await;
        assert!(ran.succeeded());
        assert_eq!(ran.text(), "hello");
        // Ran and refused: not the same finding as "not installed".
        let refused = probe("sh", &["-c", "echo nope >&2; exit 3"]).await;
        assert!(!refused.succeeded());
        assert_eq!(refused.text(), "nope", "stderr is merged, not dropped");
    }

    #[tokio::test]
    async fn a_probe_that_hangs_is_a_timeout_rather_than_a_hang() {
        let slow = probe_within("sh", &["-c", "sleep 5"], Duration::from_millis(200)).await;
        assert_eq!(slow, Probe::TimedOut);
    }

    #[test]
    fn first_line_skips_blanks_and_merges_both_streams() {
        assert_eq!(first_line(b"\n\n  answer  \nmore", b""), "answer");
        assert_eq!(first_line(b"", b"from stderr"), "from stderr");
        assert_eq!(first_line(b"", b""), "(no output)");
    }

    /// A host whose images are already built runs scouts perfectly well with
    /// none of this installed — which is every machine that received a bundle
    /// rather than a checkout.
    #[tokio::test]
    async fn the_toolchain_section_never_fails() {
        let section = toolchain_section().await;
        assert!(!section.checks.is_empty());
        for check in &section.checks {
            assert_ne!(
                check.level,
                Level::Fail,
                "the build toolchain is not a scout precondition: {check:?}"
            );
        }
    }

    /// "Nothing resolves" and "we cannot tell" are the same observation and
    /// opposite advice. An unopenable store must not send an operator to
    /// re-seal what is already sealed.
    #[test]
    fn an_unopenable_store_skips_the_credentials_rather_than_failing_them() {
        let dir = tempfile::tempdir().unwrap();
        let opts = DoctorOptions {
            data_dir: dir.path().to_path_buf(),
            strict: false,
            probe_images: false,
            env_sources: Vec::new(),
        };
        let section = credentials_section(
            &opts,
            &crate::secrets::Secrets::unresolvable(),
            Some("unseal key unavailable: no such item"),
        );
        let by_name = |name: &str| {
            section
                .checks
                .iter()
                .find(|c| c.name == name)
                .unwrap_or_else(|| panic!("no check named {name}"))
                .clone()
        };
        assert_eq!(by_name("unseal key").level, Level::Fail);
        assert!(by_name("unseal key").fix.is_some());
        assert_eq!(by_name("anthropic-api-key").level, Level::Skip);
        assert_eq!(by_name("github-token").level, Level::Skip);
    }

    /// With the store readable, the credential lines say *which source*
    /// answered — and the type that says so has no value in it, so no
    /// rendering of this can leak a key.
    #[test]
    fn credential_lines_name_a_source_and_never_a_value() {
        let dir = tempfile::tempdir().unwrap();
        let opts = DoctorOptions {
            data_dir: dir.path().to_path_buf(),
            strict: false,
            probe_images: false,
            env_sources: Vec::new(),
        };
        let secrets = crate::secrets::Secrets::for_tests(Some("ghp_secret_value"), None);
        let section = credentials_section(&opts, &secrets, None);
        let rendered = format!("{:?}", section);
        assert!(
            rendered.contains("GITHUB_TOKEN in the environment"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("ghp_secret_value"),
            "a credential reached the report"
        );
    }

    #[test]
    fn a_missing_store_is_a_note_rather_than_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let opts = DoctorOptions {
            data_dir: dir.path().to_path_buf(),
            strict: false,
            probe_images: false,
            env_sources: Vec::new(),
        };
        let secrets = crate::secrets::Secrets::for_tests(Some("t"), Some("k"));
        let section = credentials_section(&opts, &secrets, None);
        let store = section
            .checks
            .iter()
            .find(|c| c.name == "sealed store")
            .unwrap();
        assert_eq!(store.level, Level::Warn);
        assert!(
            store.fix.is_none(),
            "there is nothing to run: env fallbacks are a supported way to run this"
        );
    }

    // --- the broker, which is the check every other one on this list is
    // blind to ---

    /// An unauthenticated 401 is the **success** condition. Every broker route
    /// demands a lease before it does anything, so this is what proves the
    /// listener is up.
    #[test]
    fn an_unauthenticated_401_is_the_pass() {
        assert_eq!(
            classify_broker_reply("HTTP/1.1 401 Unauthorized\r\n\r\na lease is required"),
            BrokerProbe::DemandedLease
        );
    }

    /// The firewall case: the connection opened and produced nothing. This is
    /// the reading a loopback probe would have missed, because loopback
    /// answered a correct 401 through the whole outage.
    #[test]
    fn a_connection_that_returns_nothing_is_the_failure() {
        assert!(matches!(classify_broker_reply(""), BrokerProbe::Silent(_)));
        assert!(matches!(
            classify_broker_reply("garbage"),
            BrokerProbe::Silent(_)
        ));
        assert_eq!(
            BrokerProbe::Silent("returned nothing".into())
                .check("192.168.64.1", 4801)
                .level,
            Level::Fail
        );
    }

    #[test]
    fn another_status_says_something_is_listening_but_maybe_not_the_broker() {
        assert_eq!(
            classify_broker_reply("HTTP/1.1 200 OK\r\n\r\n"),
            BrokerProbe::SpokeHttp(200)
        );
        assert_eq!(BrokerProbe::SpokeHttp(200).check("h", 1).level, Level::Warn);
    }

    /// A bridge gateway that does not exist yet is not a broken broker — on a
    /// cold machine it is the ordinary answer, and a skip never sets the exit
    /// code.
    #[test]
    fn an_unreachable_gateway_is_a_skip_not_a_failure() {
        let check = BrokerProbe::Unreachable("no route to host".into()).check("192.168.64.1", 4801);
        assert_eq!(check.level, Level::Skip);
        assert!(check.detail.contains("bridge gateway"));
    }

    /// Real listener, real socket — the repo's own convention. A server that
    /// answers 401 the way the broker does reads as healthy.
    #[tokio::test]
    async fn probe_reads_a_real_listener_that_demands_a_lease() {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = socket
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 19\r\n\r\na lease is required",
                )
                .await;
        });
        assert_eq!(
            probe_broker("127.0.0.1", port).await,
            BrokerProbe::DemandedLease
        );
    }

    /// And the outage: a listener that accepts and says nothing.
    #[tokio::test]
    async fn probe_reads_a_real_listener_that_accepts_and_says_nothing() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            // Accept, hold the connection open, write nothing — which is what
            // the application firewall's severed listener did.
            drop(socket);
        });
        // A close and a hang both land here, and neither may be read as
        // "unreachable": the connect succeeded, so the address is fine and
        // the listener is not.
        assert!(
            matches!(
                probe_broker("127.0.0.1", port).await,
                BrokerProbe::Silent(_)
            ),
            "a listener that accepts and produces no HTTP is a failure, not a skip"
        );
    }

    #[tokio::test]
    async fn probe_reads_a_closed_port_as_refused() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(
            matches!(
                probe_broker("127.0.0.1", port).await,
                BrokerProbe::Refused(_)
            ),
            "a closed port is `nothing is listening`, not `unreachable`"
        );
    }

    /// The verify target dir routinely does not exist — it is created on first
    /// use — and "no such directory" is not an answer to "is there room".
    #[test]
    fn free_disk_walks_to_the_nearest_existing_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("not").join("created").join("yet");
        assert!(!missing.exists());
        assert!(
            free_disk_mb(&missing).is_some(),
            "a directory that does not exist yet still sits on a filesystem"
        );
    }

    /// The report is the same shape on a broken machine as on a working one:
    /// every section is present, and a reader can tell "not asked" from "not
    /// present".
    #[test]
    fn the_summary_counts_every_level_it_should() {
        let report = report_of(vec![
            Check::fail("a", "x", "fix"),
            Check::warn("b", "y", "fix"),
            Check::skip("c", "z"),
            Check::ok("d", "w"),
        ]);
        let summary = report.to_string();
        assert!(summary.contains("1 failure(s)"), "{summary}");
        assert!(summary.contains("1 warning(s)"), "{summary}");
        assert!(summary.contains("1 not asked"), "{summary}");
    }
}
